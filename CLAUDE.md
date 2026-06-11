# CLAUDE.md — subrosa

Rust CLI + Claude Code plugin: persistent local memory for Claude Code. Read this first when working here.

## How it fits together

One binary, `subrosa`. The plugin (`.claude-plugin/`, `hooks/hooks.json`) wires Claude Code's SessionStart/SessionEnd/UserPromptSubmit events to `hooks/run.sh`, which finds (or bootstraps) the binary and runs `subrosa hook <event>`. SessionStart catch-up-ingests changed transcripts and prints the checkpoint nudge; SessionEnd archives the ended session, queues it for checkpointing, and takes a throttled backup; UserPromptSubmit injects relevant past-session hits into context. On top of the archive sit curated facts: `subrosa fact` mutates them, `subrosa generate` renders a byte-budgeted MEMORY.md, and the bundled `/subrosa:checkpoint` + `/subrosa:checkpoint-backlog` skills (in `skills/`) drive the distillation workflow. `subrosa search` queries the archive; bare `subrosa` is the dashboard.

## Module map (src/)

| File | Job |
|---|---|
| main.rs | clap dispatch + small command runners |
| paths.rs | data locations, env overrides, KEY=VALUE config file |
| db.rs | schema (compatibility-critical), connect/connect_readonly, migrate, now_iso, encode_cwd, current_memdir |
| redact.rs | secret masking before storage |
| ingest.rs | JSONL flatten → turns rows, sweep, checkpoint queue |
| search.rs | FTS5 query building + result output |
| recall.rs | UserPromptSubmit relevance gate + context injection |
| facts.rs | curated facts CRUD, frontmatter parsing, type weights |
| generate.rs | byte-budgeted MEMORY.md from the facts table |
| import_existing.rs | one-time import of a MEMORY.md + leaves into the facts table |
| session.rs | session dump + checkpoint queue ops (drop/enqueue/mark-current) |
| stats.rs | dashboard (also the bare `subrosa` default) |
| backup.rs | throttled snapshots via SQLite backup API + mirror copy |
| setup.rs | interactive first-run config (mirror folder question) |
| hook.rs | hook entrypoints: stdin JSON in, log to file, always exit 0 |

## Invariants (the load-bearing decisions)

- **Schema and output formats are compatibility-critical.** Existing archives must keep working across versions. The stored-text, session-dump, MEMORY.md, and recall formats are pinned byte-for-byte by the golden tests in `tests/` — a failing golden test means a format change that needs a deliberate decision, never a quick golden-file update.
- **Hooks never fail, never block.** They log to `$SUBROSA_DIR/hook.log` and exit 0 no matter what. Stdout is reserved for intentional context injection (the session-start nudge, recall hits) — never error noise. They never spawn `claude` (recursion).
- **The live DB never goes in a synced folder.** iCloud/Dropbox-style sync corrupts live SQLite WAL/SHM sidecars mid-write. Only static snapshot files mirror out (backup.rs). Don't add anything that moves the live DB.
- **Redact before write.** Any new path that stores transcript text must go through `redact::redact`.
- **Recall must stay quiet and read-only.** It opens the DB read-only, gates on distinctive terms, and injects nothing on a weak match. Its `[subrosa recall]` header is filtered on ingest (NOISE_PREFIXES) so injections never feed back into the archive.
- **Phrase-quote FTS queries.** Hyphenated identifiers (`my-app-prod`, `TICKET-123`) trip FTS5's column/NOT syntax unless each term is quoted — `build_match` handles it; `--raw` is the opt-out.
- **The `project` column stores Claude Code's own directory encoding as-is** (the transcript's parent dir name). Don't normalize or decode it — it has to match what Claude Code writes.
- **Stay at 5 crates** (clap, regex, rusqlite, serde, serde_json). Single static binary with a small supply chain is part of the product; a new dependency needs a strong reason.

## Working on it

- `mise install` pins the toolchain. Keep it latest stable; bump deliberately and commit `mise.lock` with it. `Cargo.lock` is committed too — CI builds `--locked`.
- `git config core.hooksPath .githooks` once per clone. The pre-commit hook runs `scripts/sweep.sh` (secret shapes, database files, stray legacy naming), then fmt/clippy/tests.
- Checks CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked`, `scripts/sweep.sh`, and a cargo-audit job.
- Always test against a throwaway dir, never live data: `SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init`.
- Smoke recipe: write a synthetic transcript `.jsonl` under `/tmp/x/projects/-tmp-demo/`, then `init` → `ingest` → `search` → `session <id>` → `fact upsert --memdir /tmp/x/memdir --leaf note.md` → `generate --memdir /tmp/x/memdir --dry-run` → pipe `{"prompt":"...","cwd":"...","session_id":"..."}` into `hook user-prompt-submit` → pipe `{"transcript_path":"…","session_id":"…"}` into `hook session-end` → check `hook.log`, `pending`, the nudge from `hook session-start`, and that secrets in the stored turns are redacted.

## Releasing

Tag `vX.Y.Z` and push the tag — GitHub Actions builds the 4 targets and publishes the release with `sha256sums.txt`. Then pin the new checksums into `hooks/sha256sums.txt` + `hooks/binary-version` (the plugin bootstrap verifies against these), and update the Homebrew formula in `ij5a/homebrew-tap` with the new version + hashes.
