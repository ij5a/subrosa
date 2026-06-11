# CLAUDE.md — subrosa

Rust CLI + Claude Code plugin: persistent local memory for Claude Code. Read this first when working here.

## How it fits together

One binary, `subrosa`. The plugin (`.claude-plugin/`, `hooks/hooks.json`) wires Claude Code's SessionStart/SessionEnd events to `hooks/run.sh`, which finds the binary and runs `subrosa hook <event>`. Those archive transcripts into a local SQLite DB with FTS5. `subrosa search` queries it; `subrosa setup` is the one-time interactive config (where backup snapshots mirror to); `subrosa backup` takes throttled consistent snapshots.

## Module map (src/)

| File | Job |
|---|---|
| main.rs | clap dispatch + small command runners |
| paths.rs | data locations, env overrides, KEY=VALUE config file |
| db.rs | schema (compatibility-critical), connect, migrate, now_iso |
| redact.rs | secret masking before storage |
| ingest.rs | JSONL flatten → turns rows, sweep, checkpoint queue |
| search.rs | FTS5 query building + result output |
| backup.rs | throttled snapshots via SQLite backup API + mirror copy |
| setup.rs | interactive first-run config (mirror folder question) |
| hook.rs | hook entrypoints: stdin JSON in, log to file, always exit 0 |

## Invariants (the load-bearing decisions)

- **Schema and stored-text format are compatibility-critical.** This is a rewrite of a proven implementation; DBs are interchangeable with it, and golden-file parity (same transcript in → byte-identical `turns` rows out) is the regression test. `dumps_pylike` in ingest.rs exists exactly for that — don't "simplify" it away.
- **Hooks never fail, never block, never print to stdout.** They log to `$SUBROSA_DIR/hook.log` and exit 0 no matter what. They never spawn `claude` (recursion).
- **The live DB never goes in a synced folder.** iCloud/Dropbox-style sync corrupts live SQLite WAL/SHM sidecars mid-write. Only static snapshot files mirror out (backup.rs). Don't add anything that moves the live DB.
- **Redact before write.** Any new path that stores transcript text must go through `redact::redact`.
- **Phrase-quote FTS queries.** Hyphenated identifiers (`my-app-prod`, `TICKET-123`) trip FTS5's column/NOT syntax unless each term is quoted — `build_match` handles it; `--raw` is the opt-out.
- **The `project` column stores Claude Code's own directory encoding as-is** (the transcript's parent dir name). Don't normalize or decode it — it has to match what Claude Code writes.
- **Stay at 5 crates** (clap, regex, rusqlite, serde, serde_json). Single static binary with a small supply chain is part of the product; a new dependency needs a strong reason.

## Working on it

- `mise install` pins the toolchain. Keep it latest stable; bump deliberately and commit `mise.lock` with it. `Cargo.lock` is committed too — CI builds `--locked`.
- Checks CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked`.
- Always test against a throwaway dir, never live data: `SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init`.
- Smoke recipe: write a synthetic transcript `.jsonl` under `/tmp/x/projects/-tmp-demo/`, then `init` → `ingest` → `search` → pipe `{"transcript_path":"…","session_id":"…"}` into `hook session-end` → check `hook.log`, `pending`, and that secrets in the stored turns are redacted.
