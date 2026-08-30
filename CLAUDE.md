# CLAUDE.md: subrosa

Rust CLI and Claude Code plugin for persistent local memory. Read this file before changing code.

## How it fits together

`subrosa` is one binary. `.claude-plugin/` and `hooks/hooks.json` connect SessionStart, SessionEnd, UserPromptSubmit, PreCompact, and Stop to `hooks/run.sh`.

`hooks/run.sh` finds or bootstraps the binary. It runs `subrosa hook <event>`.

- SessionStart catch-up-ingests changed transcripts and prints the checkpoint nudge.
- SessionEnd archives the ended session, queues it, and takes a throttled backup.
- SessionStart and SessionEnd start detached `embed --auto` and do not wait for it.
- UserPromptSubmit injects relevant past-session hits into context.
- UserPromptSubmit also adds a per-prompt backlog directive while sessions wait for checkpoint. This repeats after the one-time SessionStart nudge scrolls away.
- PreCompact archives the conversation before compaction and resets recall deduplication.
- Stop incrementally ingests the live transcript after each assistant turn. It resumes from a saved byte offset, so cost stays flat as the session grows. It does not enqueue a checkpoint or take a backup.
- The live session is searchable before it ends. `subrosa fact` changes curated facts, and `subrosa generate` writes a byte-budgeted `MEMORY.md`.
- `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` drive fact distillation. `subrosa search` queries the archive, and bare `subrosa` shows the dashboard.

## Module map (src/)

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
| `timeutil.rs` | ISO-8601 and Unix-epoch helpers without chrono: `parse`, `now`, `civil_to_days`, `civil_from_days`, `parse_ymd`, and `next_day`. Stats, recall, search, and sessions use them. |
| `backup.rs` | Throttled snapshots through the SQLite backup API and plain or encrypted mirror copies |
| `crypt.rs` | Encrypted mirror snapshots with XChaCha20-Poly1305 and argon2id. It also implements `subrosa restore`. |
| `setup.rs` | Interactive first-run config for a mirror folder and optional mirror passphrase |
| `embed.rs` | CPU `bge-small-en-v1.5` model, pinned revision, one-time system-`curl` download, per-file sha256 checks on download, pinned-size checks later, CLS pooling, cosine normalization, and shared `Embedder`. `embed(&self)` is Send+Sync, so workers share one loaded model. It also owns `spawn_if_due`, `embed.lock`, and `embed.state`. |
| `wordpiece.rs` | Hand-rolled BERT WordPiece tokenizer over `vocab.txt`. The project leaves out `tokenizers` because it builds C oniguruma. |
| `hook.rs` | Hook entrypoints. They read JSON from stdin, log to a file, always exit 0, and start the detached indexer at SessionStart and SessionEnd. |

## Invariants

- **Schema and output formats are compatibility-critical.** Existing archives must work across versions. Golden tests pin stored text, session dumps, `MEMORY.md`, recall, related, fact links, and session listings byte for byte. A failing golden test needs a deliberate format decision. Never update a golden file only to silence a failure.
  - Schema changes are additive through `migrate()`. v3 added `session_tags` and its backfill. v4 added the `scan_offset` and `scan_seq` ingest cursor.
- **Hooks never fail or block.** They log to `$SUBROSA_DIR/hook.log` and always exit 0. Stdout is only for intentional context injection: the session-start nudge, the per-prompt backlog directive, and recall hits. Never print error noise there. Never spawn `claude`, because that would recurse.
- **The live database never goes in a synced folder.** iCloud and Dropbox-style sync can corrupt SQLite WAL and SHM sidecars during a write. Only static snapshot files may mirror. Do not move the live database.
- **An intended encrypted mirror never becomes plaintext.** Intent starts when a passphrase resolves, even to an error, or when `subrosa-latest.db.enc` already exists. Any later failure skips the mirror and leaves it stale. It never falls back to a readable copy. Clear the plaintext twin before any bailout. Turning encryption off requires manually deleting `.enc`; missing config must not do it.
- **The binary has no network path except the model download child.** The one-time download uses a system `curl` child, a pinned revision, and sha256 checks. Nothing else in the tree reaches the network, and no text is uploaded. Hooks never download or embed. SessionStart and SessionEnd spawn `embed --auto` in a detached process group with null stdio and no wait. Claude Code kills hooks at 120 seconds, while a first backfill takes minutes.
  - `semantic=off` in config or `SUBROSA_SEMANTIC` stops the spawn and the download. `ensure_model` refuses before `curl`. A model already on disk may still load. An unreadable config counts as off, so it fails closed.
  - A failed run writes `embed.state`. The next spawn waits for its retry window, so an offline machine does not retry every session.
  - Embedding runs in-process on the CPU. `subrosa embed` and `search --semantic` construct `Embedder` directly. Plain search constructs it lazily only after an eligible exact miss. Recall, hooks, and ingest keep their startup cost. `--semantic` fails loudly instead of falling back to keyword.
  - Redact turns before embedding at ingest. Redact the query through `redact::redact` too.
  - `turn_embeddings` is a lazy table like the trigram index. Create it outside `migrate()`, so `SCHEMA_VERSION` stays unchanged. Delete vectors for another model key before backfill, because they cannot rank or resume.
- **System tools use absolute paths, never `PATH`.** `curl`, `stty`, and `git` resolve through `paths::system_tool` and fixed absolute paths. `SUBROSA_CURL` is the deliberate override. A missing tool degrades the feature: no download, an echoing passphrase prompt, or no repository label. Never look up these tools through `PATH`.
- **Small control files use `paths::read_control_file`.** This includes config, `embed.state`, the checkpoint queue, `.budget`, the recall dedup log, and `MEMORY.md`. The helper accepts regular files only, resolves symlinks by hand, and caps size. A FIFO could block a hook forever. A dangling symlink returns `ENOENT`, which prevents an evicted cloud-synced `semantic=off` from becoming on again.
  - `Ok(None)` means nothing exists. `Err` means an existing file is unusable. Callers must keep those cases separate.
- **Redact before writing.** Every new path that stores transcript text must call `redact::redact`.
- **Recall stays quiet and read-only.** It opens the database read-only, requires distinctive terms, and injects nothing for a weak match. Ingest filters the `[subrosa recall]` header with `NOISE_PREFIXES`, so injected text cannot feed back into the archive.
- **Quote every FTS phrase.** Hyphenated identifiers such as `my-app-prod` and `TICKET-123` can trigger FTS5 column or `NOT` syntax. `build_match` quotes each term. `--raw` is the opt-out.
- **Keep the `project` column unchanged.** It stores Claude Code's directory encoding from the transcript parent directory. Do not normalize or decode it, because it must match Claude Code's value.
- **Stay at 11 direct crates on every platform.** They are clap, regex, rusqlite, serde, serde_json, chacha20poly1305, argon2, candle-core, candle-nn, candle-transformers, and sha2. jp approved each batch. Rolling our own AEAD, KDF, or transformer was the only alternative. macOS re-declares candle-core with its `accelerate` feature. `accelerate-src` arrives transitively and is not a 12th direct crate. The single static binary and small supply chain are product requirements, so a new dependency needs a strong reason.
  - Apart from rusqlite's bundled SQLite, nothing compiles C. Linux and musl builds use no platform libraries. macOS links only Apple's built-in Accelerate framework. The `accelerate-src` build script has one `rustc-link-lib` line.
  - Releases build 4 targets, including musl. Keep candle at 0.9 because 0.10 and later hard-depend on `tokenizers`, which pulls `onig_sys` and bundled C oniguruma. A guard test in `embed.rs` reads `Cargo.lock` and fails if either name returns.

## Working on it

- `mise install` pins the toolchain. Keep it on the latest stable release. Bump it deliberately and commit `mise.lock`. Commit `Cargo.lock` too, because CI uses `--locked`.
- Run `git config core.hooksPath .githooks` once per clone. The pre-commit hook runs `scripts/sweep.sh` for secret shapes, database files, and stray legacy names. It then runs format, clippy, and tests.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`, `scripts/sweep.sh`, and cargo audit. It has no separate `cargo build --locked` step.
- Use a throwaway directory for tests: `mise exec -- env SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init`.

Smoke recipe:

1. Write a synthetic `.jsonl` file under `/tmp/x/projects/-tmp-demo/`.
2. Run `init`, `ingest`, and `search`. Try `--after`, `--before`, and `--tag`.
3. Run `sessions --tag tool:bash`, `session <id> --tags`, and the fact commands: `fact upsert --memdir /tmp/x/memdir --leaf note.md`, `fact doctor --memdir /tmp/x/memdir`, and `generate --memdir /tmp/x/memdir --dry-run`.
4. Pipe `{"prompt":"...","cwd":"...","session_id":"..."}` into `hook user-prompt-submit`.
5. Pipe `{"transcript_path":"...","session_id":"..."}` into `hook stop`. Then run `hook session-end`.
6. Export `SUBROSA_SEMANTIC=off` unless you are testing the indexer. Otherwise the hooks start a real model download.
7. Check `hook.log`, `pending`, the `hook session-start` nudge, redacted stored secrets, and derived tags.

## Verification gate (before commit, push, and release)

Run every gate for every code change. Do not skip a gate because a change looks safe. The pre-commit hook and CI run the tests. Run the other gates by hand.

Gates 1 through 4 are hard release requirements wrapped by `scripts/release-check.sh`. Run them on the final tree, the exact commit you will tag, and the built binary. Earlier results do not cover later changes.

1. **Regression:** Run `cargo test --locked`. This runs unit tests and golden tests. A red test blocks the commit. Golden changes need a deliberate decision.
2. **Performance:** Run `scripts/bench.sh`. It needs `hyperfine` and covers recall, search, ingest, and startup. The README and FAQ promise its latency numbers. Run it before push or release and after changes to `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Token usage:** `scripts/bench.sh` also measures recall injection. It fails above the 220-token guard behind the about-180-token promise. Keep this as a separate gate because per-prompt cost is a product requirement.
4. **Smoke:** Run `scripts/smoke.sh` with the built binary. It uses a throwaway directory and checks redaction of stored turns, encrypted-mirror and restore paths, a fail-closed budget override, and hook exit 0. Unit tests do not replace it.
   - Run `scripts/detach-test.sh` by hand after spawn-path changes. It needs shell job control and is not part of `scripts/release-check.sh`. It proves the background indexer survives the session that starts it.
5. **Security review:** Run `cargo audit` and `/security-review` over the branch diff. Do this before pushing code and before every release because the repository is public.
6. **Docs in sync:** Update every affected `README.md`, `docs/*.md`, `CLAUDE.md`, and skill document with the behavior change. Keep token, latency, flag, and limit claims equal to the code.

Gates 5 and 6 need manual work. Before a public release, run all 6 gates on the final commit. Never tag or push an unverified or undocumented tree.

## Releasing

Run `scripts/release-check.sh` on the exact commit you will tag. Complete the security review and docs check by hand.

Bump the version in both `Cargo.toml` and `.claude-plugin/plugin.json`. Keep the versions equal. The plugin uses `plugin.json` to show and detect updates.

Add the new version section to `CHANGELOG.md` in the same commit before the tag. Add its `[x.y.z]:` compare-link definition at the bottom. Without that definition, the version heading renders as literal text.

Tag `vX.Y.Z` and push the tag. GitHub Actions builds 4 targets and publishes `sha256sums.txt`.

Pin the new hashes in `hooks/sha256sums.txt` and `hooks/binary-version`. Update the Homebrew formula in `ij5a/homebrew-tap` with the version and hashes.

Update the local PATH binary with `cargo install --git https://github.com/ij5a/subrosa --tag vX.Y.Z --locked --force`. Confirm `subrosa -V`. `hooks/run.sh` prefers a PATH binary over bootstrapping one, so a stale binary can keep the plugin on old code.
