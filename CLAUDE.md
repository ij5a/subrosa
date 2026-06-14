# CLAUDE.md — subrosa

Rust CLI + Claude Code plugin: persistent local memory for Claude Code. Read this first when working here.

## How it fits together

One binary, `subrosa`. The plugin (`.claude-plugin/`, `hooks/hooks.json`) wires Claude Code's SessionStart/SessionEnd/UserPromptSubmit/PreCompact events to `hooks/run.sh`, which finds (or bootstraps) the binary and runs `subrosa hook <event>`. SessionStart catch-up-ingests changed transcripts and prints the checkpoint nudge; SessionEnd archives the ended session, queues it for checkpointing, and takes a throttled backup; UserPromptSubmit injects relevant past-session hits into context; PreCompact archives the conversation before compaction summarizes it away and resets recall dedup so post-compact prompts can re-inject. On top of the archive sit curated facts: `subrosa fact` mutates them, `subrosa generate` renders a byte-budgeted MEMORY.md, and the bundled `/subrosa:checkpoint` + `/subrosa:checkpoint-backlog` skills (in `skills/`) drive the distillation workflow. `subrosa search` queries the archive; bare `subrosa` is the dashboard.

## Module map (src/)

| File | Job |
|---|---|
| main.rs | clap dispatch + small command runners |
| paths.rs | data locations, env overrides, KEY=VALUE config file |
| db.rs | schema (compatibility-critical), connect/connect_readonly, migrate, now_iso, encode_cwd, current_memdir |
| redact.rs | secret masking before storage |
| ingest.rs | JSONL flatten → turns rows, sweep, checkpoint queue |
| search.rs | FTS5 query building + result output |
| related.rs | `related` verb: co-occurrence over the archive (anchor → terms + sessions; FTS-count idf down-weight) |
| recall.rs | UserPromptSubmit relevance gate + context injection |
| text.rs | shared tokenizer/term-quality helpers (STOPWORDS, extract_terms, is_anchor, turn_tokens, token_matches); used by recall + related |
| facts.rs | curated facts CRUD, frontmatter parsing, type weights |
| generate.rs | byte-budgeted MEMORY.md from the facts table |
| import_existing.rs | one-time import of a MEMORY.md + leaves into the facts table |
| session.rs | session dump (full id or unique prefix) + checkpoint queue ops (drop/enqueue/mark-current) |
| stats.rs | dashboard (also the bare `subrosa` default) |
| timeutil.rs | ISO-8601 ↔ Unix-epoch helpers (no chrono); shared by stats + recall |
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

## Verification gate (before commit, push, and release)

Non-negotiable for any code change — never skip a step because it "looks safe". Tests are wired into the pre-commit hook (`.githooks/pre-commit`) and CI; the other three are run by hand, and all four must be green before anything reaches the public.

1. **Tests** — `cargo test --locked` (unit + the golden tests in `tests/`). A red test blocks the commit. Golden-format changes are deliberate: update the golden on purpose, never to silence a failure.
2. **Performance** — `scripts/bench.sh` (needs `hyperfine`; covers the recall hot path, search, ingest, startup). The latency numbers are a product promise the README/FAQ cite, so a regression is a defect. Run it before push/release and on any change touching `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Security review** — `cargo audit` (dependency CVEs, also a CI job) plus `/security-review` over the branch diff. Run before pushing code and always before a release — this repo is public.
4. **Docs in sync** — update every affected markdown file (`README.md`, `docs/*.md`, `CLAUDE.md`, skill docs) in the same change as the behavior it documents. Token/latency claims, flags, and limits in the docs must match the code; a code change that lands without its doc update is incomplete.

Before a public release, run all four green, then follow the steps below. Never tag or push a release on an unverified or undocumented tree.

## Releasing

First bump the version in **both** `Cargo.toml` and `.claude-plugin/plugin.json` — they must match. `plugin.json` is the version `/plugin` shows and uses to detect updates, so bumping `Cargo.toml` alone ships a release no installed plugin will pull. Add the new version's section to `CHANGELOG.md` in that same version-bump commit (before the tag, so the tagged tree carries it). Then tag `vX.Y.Z` and push the tag — GitHub Actions builds the 4 targets and publishes the release with `sha256sums.txt`. Then pin the new checksums into `hooks/sha256sums.txt` + `hooks/binary-version` (the plugin bootstrap verifies against these), and update the Homebrew formula in `ij5a/homebrew-tap` with the new version + hashes. Last, update the local CLI on PATH to match — `cargo install --git https://github.com/ij5a/subrosa --tag vX.Y.Z --locked --force`, then confirm `subrosa -V`. This step is not optional: `hooks/run.sh` prefers a `subrosa` already on PATH over bootstrapping its own, so a stale PATH binary (e.g. an old `cargo install`) silently keeps the plugin running old code even after `/plugin` shows the new version.
