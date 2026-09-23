# CLAUDE.md: subrosa

Rust CLI and Claude Code plugin for persistent local memory. Read before changing code.

## How it fits together

`subrosa` is one binary. `.claude-plugin/` and `hooks/hooks.json` connect SessionStart, SessionEnd, UserPromptSubmit, PreCompact, and Stop to `hooks/run.sh`, which finds or bootstraps it and runs `subrosa hook <event>`.

- SessionStart catch-up-ingests changed transcripts and prints the checkpoint nudge.
- SessionEnd starts a detached worker that archives, queues, retries SQLite contention for a bounded time, optionally drains up to 3 checkpoints, and backs up. The hook returns immediately. Sweep recovers a missed worker.
- SessionStart and SessionEnd start detached `embed --auto`.
- UserPromptSubmit injects past-session hits and repeats backlog directives unless distill is enabled.
- PreCompact archives before compaction and resets deduplication.
- Stop ingests after each assistant turn from a saved byte offset. It does not enqueue or back up.
- The live session is searchable before it ends. `subrosa fact` changes facts; `subrosa generate` writes byte-limited `MEMORY.md`.
- `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` distill facts. `subrosa search` queries the archive. Bare `subrosa` shows the dashboard.

## Module map

| File | Job |
|---|---|
| `main.rs` | clap dispatch and small command runners |
| `paths.rs` | Data paths, environment overrides, and the `KEY=VALUE` config. It handles `semantic` and `embed.state`. |
| `db.rs` | Compatibility-critical schema, `connect`, `connect_readonly`, `migrate()`, `now_iso`, `encode_cwd`, `current_memdir`, and lazy trigram and `turn_embeddings` tables. The schema includes `session_tags`. |
| `redact.rs` | Secret masking before storage |
| `ingest.rs` | JSONL to turn rows, seek-resume ingest with `scan_offset` and `scan_seq`, sweep, checkpoint queue, and tag derivation hook |
| `search.rs` | FTS5 queries and output with `--after`, `--before`, and `--tag`; semantic ranking; and `subrosa embed` backfill. Backfill uses newest-first slabs, one thread per core, deduplication, one shared `Embedder`, and one DB writer. `--auto` uses half the cores, with a floor of 2 and a cap at the core count. A 2 or 3 core machine uses 2 cores. A 1 core machine uses 1. |
| `sessions.rs` | `sessions`: list sessions newest-first and filter by project, date, or tag |
| `related.rs` | `related`: co-occurrence from an anchor to terms and sessions, with FTS-count IDF down-weighting |
| `recall.rs` | UserPromptSubmit relevance gate and context injection |
| `text.rs` | Shared tokenizer and term-quality helpers: `STOPWORDS`, `extract_terms`, `is_anchor`, `turn_tokens`, and `token_matches`. Recall, related, and tags use them. |
| `tags.rs` | Deterministic, read-only `tool:`, `ext:`, and `topic:` tags. `derive_tags` runs at ingest, and `backfill` runs at schema v3. |
| `facts.rs` | Curated facts CRUD, frontmatter parsing, type weights, `fact link` `[[name]]` graph reads, and read-only `fact doctor` leaf and row checks |
| `generate.rs` | Byte-budgeted `MEMORY.md`. It supports `<memdir>/.budget` and stops at Claude Code's 200-line load limit. |
| `import_existing.rs` | One-time import of a `MEMORY.md` and its leaves into the facts table |
| `session.rs` | Session dump by full ID or unique prefix, optional `--tags`, and checkpoint queue operations: drop, enqueue, and mark-current |
| `stats.rs` | Dashboard, including the semantic-index progress line. It is also the bare `subrosa` command. |
| `timeutil.rs` | ISO-8601 and Unix-epoch helpers without chrono: `parse_ts`, `now_unix`, `civil_to_days`, `civil_from_days`, `parse_ymd`, and `next_day`. Stats, recall, search, and sessions use them. |
| `backup.rs` | Throttled snapshots through the SQLite backup API and plain or encrypted mirror copies |
| `crypt.rs` | Encrypted mirror snapshots with XChaCha20-Poly1305 and argon2id. It also implements `subrosa restore`. |
| `setup.rs` | Interactive first-run config for a mirror folder and optional mirror passphrase |
| `embed.rs` | CPU `bge-small-en-v1.5` model, pinned revision, one-time system-`curl` download, per-file sha256 checks on download, pinned-size checks later, CLS pooling, cosine normalization, and shared `Embedder`. `embed(&self)` is Send+Sync, so workers share one loaded model. It also owns `spawn_if_due`, `embed.lock`, and `embed.state`. |
| `distill.rs` | Opt-in detached checkpoint worker, its run lock and retry state, muted child session ids, and prove-before-drop queue completion. |
| `wordpiece.rs` | Hand-rolled BERT WordPiece tokenizer over `vocab.txt`. The project leaves out `tokenizers` because it builds C oniguruma. |
| `hook.rs` | Hook entrypoints. They read JSON from stdin, log to a file, always exit 0, and start the detached indexer at SessionStart and SessionEnd. |

## Invariants

- **Schema and output formats are compatibility-critical.** Archives must work across versions. Golden tests pin stored text, session dumps, `MEMORY.md`, recall, related, fact links, and session listings byte for byte. A failing golden test needs a deliberate format decision. Never update one only to silence failure.
  - Schema changes are additive through `migrate()`. v3 adds `session_tags` and backfill. v4 adds `scan_offset` and `scan_seq`. v5 imports the old queue once. v6 adds the queue ordering column.
- **Hooks never fail or block.** They log to `$SUBROSA_DIR/hook.log` and exit 0. SessionEnd hands archive, queue writes, and optional distill spawning to detached workers. Stdout carries only the nudge, backlog directive, and recall hits. Hooks never spawn Claude; the detached distill worker may, only with `--bare` and a muted session id.
- **Checkpoint completion is monotonic.** `checkpointed_seq` only moves forward. `checkpoint-clear --confirm` refuses a queue that still needs per-session verification. `checkpoint-drop` without `--max-seq` uses the distilled watermark as its boundary; pass `--max-seq` to acknowledge a verified prefix explicitly.
- **The live database never goes in a synced folder.** Sync can corrupt SQLite WAL and SHM sidecars. Only static snapshots may mirror. Do not move the live database.
- **An intended encrypted mirror never becomes plaintext.** Intent starts when a passphrase resolves, even with an error, or when `subrosa-latest.db.enc` exists. Later failure skips the mirror and leaves it stale. It never falls back. Clear the plaintext twin before bailout. Disable encryption by deleting `.enc`; missing config must not do it.
- **The binary has no network path except the model download child and the opt-in distill child.** The one-time download uses system `curl`, a pinned revision, and sha256 checks. The distill child sends redacted transcript text to Anthropic, so `distill` is unset and off by default. Hooks never download, embed, or distill. SessionStart and SessionEnd spawn background work in detached process groups with null stdio and no wait.
  - `semantic=off` or `SUBROSA_SEMANTIC=off` stops spawn and download. Other values keep the feature on. `ensure_model` refuses before `curl`. A disk model may load. An unreadable config counts as off.
  - A failed run writes `embed.state`. The next spawn waits for its retry window; an offline machine does not retry every session.
  - Embedding runs in-process on the CPU. `subrosa embed` and `search --semantic` construct `Embedder` directly. Plain search constructs it only after an eligible exact miss. Recall, hooks, and ingest keep their startup cost. `--semantic` fails instead of falling back.
  - Redact turns before embedding at ingest. Redact the query through `redact::redact`.
  - `turn_embeddings` is lazy like the trigram index. Create it outside `migrate()` so `SCHEMA_VERSION` stays unchanged. Delete vectors for another model key before backfill; they cannot rank or resume.
- **System tools use absolute paths, never `PATH`.** `curl`, `stty`, and `git` resolve through `paths::system_tool`. `SUBROSA_CURL` overrides it. A missing tool means no download, a refused passphrase prompt, or no repository label. Never use `PATH`.
- **Small control files use `paths::read_control_file`.** This includes config, `embed.state`, `.budget`, the recall log, and `MEMORY.md`. SQLite holds the queue; v5 alone reads the legacy log. The helper accepts regular files, resolves symlinks by hand, and caps size. A FIFO could block a hook. A dangling symlink returns `ENOENT`, keeping evicted `semantic=off` off.
  - `Ok(None)` means absent. `Err` means unusable. Keep these cases separate.
- **Redact before writing.** Every path storing transcript text must call `redact::redact`.
- **Recall stays quiet and read-only.** It opens the database read-only, requires distinctive terms, and injects nothing for weak matches. Ingest filters `[subrosa recall]` with `NOISE_PREFIXES`, preventing feedback.
- **Quote every FTS phrase.** Hyphenated identifiers such as `my-app-prod` and `TICKET-123` can trigger FTS5 syntax. `build_match` quotes each term. `--raw` opts out.
- **Keep `project` unchanged.** It stores Claude Code's encoded transcript-parent directory. Do not normalize or decode it.
- **Stay at 11 direct crates on every platform.** They are clap, regex, rusqlite, serde, serde_json, chacha20poly1305, argon2, candle-core, candle-nn, candle-transformers, and sha2. jp approved each batch. Rolling our own AEAD, KDF, or transformer was the only alternative. macOS re-declares candle-core with `accelerate`; `accelerate-src` is transitive, not a 12th crate. The static binary and small supply chain require a strong reason for new dependencies.
- Apart from rusqlite's bundled SQLite, nothing compiles C. Linux and musl use no platform libraries. macOS links Apple's built-in Accelerate framework. The `accelerate-src` build script has one `rustc-link-lib` line.
  - Releases build 4 targets, including musl. Keep candle at 0.9 because 0.10 and later hard-depend on `tokenizers`, which pulls `onig_sys` and bundled C oniguruma. A guard test in `embed.rs` reads `Cargo.lock` and fails if either name returns.

## Working on it

- `mise install` pins the toolchain. Keep the latest stable release. Bump deliberately. Commit `mise.lock` and `Cargo.lock`; CI uses `--locked`.
- Run `git config core.hooksPath .githooks` once per clone. The hook runs `scripts/sweep.sh` for secrets, database files, and legacy names, then format, clippy, and tests.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`, `scripts/sweep.sh`, and cargo audit. It has no separate `cargo build --locked` step.
- Use a throwaway directory for tests: `mise exec -- env SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init`.

Smoke recipe:

1. Write a synthetic `.jsonl` file under `/tmp/x/projects/-tmp-demo/`.
2. Run `init`, `ingest`, and `search`. Try `--after`, `--before`, and `--tag`.
3. Run `sessions --tag tool:bash`, `session <id> --tags`, and the fact commands: `fact upsert --memdir /tmp/x/memdir --leaf note.md`, `fact doctor --memdir /tmp/x/memdir`, and `generate --memdir /tmp/x/memdir --dry-run`.
4. Pipe `{"prompt":"...","cwd":"...","session_id":"..."}` into `hook user-prompt-submit`.
5. Export `SUBROSA_SEMANTIC=off` before running hooks unless you are testing the indexer. Otherwise the hooks can start a model download.
6. Pipe `{"transcript_path":"...","session_id":"..."}` into `hook stop`. Then run `hook session-end`.
7. Check `hook.log`, `pending`, the `hook session-start` nudge, redacted stored secrets, and derived tags.

## Verification gate

Run every gate for every code change. Do not skip one because a change looks safe. The hook and CI run tests; run other gates by hand.

Gates 1 through 4 are hard release requirements wrapped by `scripts/release-check.sh`. Run them on the final tree, tagged commit, and binary. Earlier results do not cover changes.

1. **Regression:** Run `cargo test --locked`. It runs unit and golden tests. A red test blocks the commit. Golden changes need a decision.
2. **Performance:** Run `scripts/bench.sh`. It needs `hyperfine` and covers recall, search, ingest, and startup. The README and FAQ promise its latency numbers. Run it before push or release and after changes to `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Token usage:** `scripts/bench.sh` measures recall injection with a bytes/3.8 estimate from 1 fixture. The 220 estimate is a benchmark guard, not a runtime limit. Keep it separate because per-prompt cost is a product requirement.
4. **Smoke:** Run `scripts/smoke.sh` with the built binary. It uses a throwaway directory and checks redaction of stored turns, encrypted-mirror and restore paths, a fail-closed budget override, and hook exit 0. Unit tests do not replace it.
   - Run `scripts/detach-test.sh` by hand after spawn-path changes. It needs shell job control and is not part of `scripts/release-check.sh`. It proves the background indexer survives the session that starts it.
5. **Security review:** Run `cargo audit` and `/security-review` over the branch diff before pushing code and every release because the repository is public.
6. **Docs in sync:** Update affected `README.md`, `docs/*.md`, `CLAUDE.md`, and skill documents. Token, latency, flag, and limit claims must match code.

Gates 5 and 6 need manual work. Before release, run all 6 gates on the final commit. Never tag or push an unverified or undocumented tree.

## Releasing

Run `scripts/release-check.sh` on the commit you will tag. Complete the security review and docs check by hand.

Bump the version in both `Cargo.toml` and `.claude-plugin/plugin.json`. Keep the versions equal. The plugin uses `plugin.json` to show and detect updates.

Add the new version section to `CHANGELOG.md` before the tag. Add its `[x.y.z]:` compare-link definition at the bottom. Without it, the heading renders as literal text.

Tag `vX.Y.Z` and push the tag. GitHub Actions builds 4 targets and publishes `sha256sums.txt`.

Pin the new hashes in `hooks/sha256sums.txt` and `hooks/binary-version`. Update the Homebrew formula in `ij5a/homebrew-tap` with the version and hashes.

Update the local PATH binary with `cargo install --git https://github.com/ij5a/subrosa --tag vX.Y.Z --locked --force`. Confirm `subrosa -V`. `hooks/run.sh` prefers a PATH binary over bootstrapping one, so a stale binary can keep the plugin on old code.
