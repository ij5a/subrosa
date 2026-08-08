# CLAUDE.md — subrosa

Rust CLI + Claude Code plugin: persistent local memory for Claude Code. Read this first when working here.

## How it fits together

One binary, `subrosa`. The plugin (`.claude-plugin/`, `hooks/hooks.json`) wires Claude Code's SessionStart/SessionEnd/UserPromptSubmit/PreCompact/Stop events to `hooks/run.sh`, which finds (or bootstraps) the binary and runs `subrosa hook <event>`. SessionStart catch-up-ingests changed transcripts and prints the checkpoint nudge; SessionEnd archives the ended session, queues it for checkpointing, and takes a throttled backup; UserPromptSubmit injects relevant past-session hits into context (and, while sessions are queued for checkpoint, a per-prompt directive to run the backlog — the one-shot SessionStart nudge gets scrolled past once the first task lands); PreCompact archives the conversation before compaction summarizes it away and resets recall dedup so post-compact prompts can re-inject; Stop incrementally ingests just the in-progress transcript after each assistant turn — resuming from a saved byte offset so per-turn cost stays flat as the session grows — so the live session is searchable before it ends (no checkpoint enqueue or backup — those stay on SessionEnd). On top of the archive sit curated facts: `subrosa fact` mutates them, `subrosa generate` renders a byte-budgeted MEMORY.md, and the bundled `/subrosa:checkpoint` + `/subrosa:checkpoint-backlog` skills (in `skills/`) drive the distillation workflow. `subrosa search` queries the archive; bare `subrosa` is the dashboard.

## Module map (src/)

| File | Job |
|---|---|
| main.rs | clap dispatch + small command runners |
| paths.rs | data locations, env overrides, KEY=VALUE config file |
| db.rs | schema (compatibility-critical, incl. `session_tags`), connect/connect_readonly, migrate, now_iso, encode_cwd, current_memdir, lazy opt-in tables (trigram index, `turn_embeddings`) |
| redact.rs | secret masking before storage |
| ingest.rs | JSONL flatten → turns rows, incremental seek-resume ingest (`scan_offset`/`scan_seq` cursor), sweep, checkpoint queue, tag derivation hook |
| search.rs | FTS5 query building + result output (incl. `--after`/`--before`/`--tag` filters), plus the opt-in `--semantic` ranking and the `subrosa embed` backfill |
| sessions.rs | `sessions` verb: list past sessions newest-first, filter by project/date/tag |
| related.rs | `related` verb: co-occurrence over the archive (anchor → terms + sessions; FTS-count idf down-weight) |
| recall.rs | UserPromptSubmit relevance gate + context injection |
| text.rs | shared tokenizer/term-quality helpers (STOPWORDS, extract_terms, is_anchor, turn_tokens, token_matches); used by recall + related + tags |
| tags.rs | auto-derived read-only session tags (`tool:`/`ext:`/`topic:`): `derive_tags` at ingest, `backfill` at schema v3; fully deterministic |
| facts.rs | curated facts CRUD, frontmatter parsing, type weights, `fact link` ([[name]] graph reader), `fact doctor` (read-only leaf+row integrity lint) |
| generate.rs | byte-budgeted MEMORY.md from the facts table; per-project `<memdir>/.budget` override, and selection also stops at Claude Code's 200-line load limit |
| import_existing.rs | one-time import of a MEMORY.md + leaves into the facts table |
| session.rs | session dump (full id or unique prefix, opt-in `--tags`) + checkpoint queue ops (drop/enqueue/mark-current) |
| stats.rs | dashboard (also the bare `subrosa` default) |
| timeutil.rs | ISO-8601 ↔ Unix-epoch helpers (no chrono): parse/now/civil_to_days/civil_from_days/parse_ymd/next_day; shared by stats + recall + search/sessions |
| backup.rs | throttled snapshots via SQLite backup API + mirror copy (plain or encrypted) |
| crypt.rs | encrypted mirror snapshots: XChaCha20-Poly1305 + argon2id, 60-byte header as AAD, `subrosa restore` |
| setup.rs | interactive first-run config (mirror folder + optional mirror passphrase) |
| embed.rs | the bundled model (bge-large-en-v1.5 on candle, CPU): pinned revision, one-time download via a system curl child, per-file sha256 checked on download only (later runs just stat the pinned sizes — rehashing 1.3 GB per search cost more than it bought), CLS pooling, cosine/normalize |
| wordpiece.rs | hand-rolled BERT WordPiece tokenizer over vocab.txt (the `tokenizers` crate is deliberately out — it builds C oniguruma) |
| hook.rs | hook entrypoints: stdin JSON in, log to file, always exit 0 |

## Invariants (the load-bearing decisions)

- **Schema and output formats are compatibility-critical.** Existing archives must keep working across versions. The stored-text, session-dump, MEMORY.md, recall, related, fact-link, and sessions-listing formats are pinned byte-for-byte by the golden tests in `tests/` — a failing golden test means a format change that needs a deliberate decision, never a quick golden-file update. Schema changes are additive only, through `migrate()` (v3 added `session_tags` + backfill; v4 added the `scan_offset`/`scan_seq` ingest resume cursor).
- **Hooks never fail, never block.** They log to `$SUBROSA_DIR/hook.log` and exit 0 no matter what. Stdout is reserved for intentional context injection (the session-start nudge, the per-prompt checkpoint-backlog directive, recall hits) — never error noise. They never spawn `claude` (recursion).
- **The live DB never goes in a synced folder.** iCloud/Dropbox-style sync corrupts live SQLite WAL/SHM sidecars mid-write. Only static snapshot files mirror out (backup.rs). Don't add anything that moves the live DB.
- **Once encryption is intended, the mirror never goes out plaintext.** Intent means a passphrase resolves (even to an error) or a `subrosa-latest.db.enc` is already there. Any failure after that skips the mirror and leaves it stale — it never falls back to a readable copy — and the plaintext twin is cleared before any bailout. Turning encryption off is a manual delete of the `.enc`, never something a missing config does on its own.
- **The binary opens no sockets.** The one-time model download runs through a system curl child (`embed.rs`), pinned to one revision and sha256-verified as it lands — nothing else in the tree reaches the network, and no text of yours is ever uploaded. Embedding runs in-process on the CPU. Only `subrosa embed` and `search --semantic` construct an `Embedder`, lazily, so recall, hooks and ingest keep their startup cost; `--semantic` fails loudly rather than falling back to keyword. Text is redacted before it reaches the model (turns at ingest, the query through `redact::redact`). Its store (`turn_embeddings`) is a lazy table like the trigram index — created on first use, outside `migrate()`, so SCHEMA_VERSION stays put.
- **Redact before write.** Any new path that stores transcript text must go through `redact::redact`.
- **Recall must stay quiet and read-only.** It opens the DB read-only, gates on distinctive terms, and injects nothing on a weak match. Its `[subrosa recall]` header is filtered on ingest (NOISE_PREFIXES) so injections never feed back into the archive.
- **Phrase-quote FTS queries.** Hyphenated identifiers (`my-app-prod`, `TICKET-123`) trip FTS5's column/NOT syntax unless each term is quoted — `build_match` handles it; `--raw` is the opt-out.
- **The `project` column stores Claude Code's own directory encoding as-is** (the transcript's parent dir name). Don't normalize or decode it — it has to match what Claude Code writes.
- **Stay at 11 crates** (clap, regex, rusqlite, serde, serde_json; chacha20poly1305 + argon2 for mirror encryption; candle-core/candle-nn/candle-transformers + sha2 for the bundled embedder — jp approved each batch, and rolling our own AEAD, KDF or transformer was the only alternative). Single static binary with a small supply chain is part of the product; a new dependency needs a strong reason. Pure Rust only, no C deps — the release builds 4 targets including musl, so **candle stays pinned at 0.9**: 0.10+ hard-depends on `tokenizers` → `onig_sys` (bundled C oniguruma). A guard test in `embed.rs` reads `Cargo.lock` and fails if either name comes back.

## Working on it

- `mise install` pins the toolchain. Keep it latest stable; bump deliberately and commit `mise.lock` with it. `Cargo.lock` is committed too — CI builds `--locked`.
- `git config core.hooksPath .githooks` once per clone. The pre-commit hook runs `scripts/sweep.sh` (secret shapes, database files, stray legacy naming), then fmt/clippy/tests.
- Checks CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked`, `scripts/sweep.sh`, and a cargo-audit job.
- Always test against a throwaway dir, never live data: `SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init`.
- Smoke recipe: write a synthetic transcript `.jsonl` under `/tmp/x/projects/-tmp-demo/`, then `init` → `ingest` → `search` (try `--after`/`--before`/`--tag`) → `sessions --tag tool:bash` → `session <id> --tags` → `fact upsert --memdir /tmp/x/memdir --leaf note.md` → `fact doctor --memdir /tmp/x/memdir` → `generate --memdir /tmp/x/memdir --dry-run` → pipe `{"prompt":"...","cwd":"...","session_id":"..."}` into `hook user-prompt-submit` → pipe `{"transcript_path":"…","session_id":"…"}` into `hook stop` (mid-session live ingest) then `hook session-end` → check `hook.log`, `pending`, the nudge from `hook session-start`, that secrets in the stored turns are redacted, and that tags were derived.

## Verification gate (before commit, push, and release)

Non-negotiable for any code change — never skip a step because it "looks safe". Tests are wired into the pre-commit hook (`.githooks/pre-commit`) and CI; the rest are run by hand, and every gate must be green before anything reaches the public.

**Four of these are hard release requirements and are wrapped by one command — `scripts/release-check.sh` — which must run green on the exact commit you are about to tag.** Running them earlier in the branch and assuming nothing changed since is not enough: run them on the final tree, against the actually-built binary.

1. **Regression** — `cargo test --locked` (unit + the golden tests in `tests/`). A red test blocks the commit. Golden-format changes are deliberate: update the golden on purpose, never to silence a failure.
2. **Performance** — `scripts/bench.sh` (needs `hyperfine`; covers the recall hot path, search, ingest, startup). The latency numbers are a product promise the README/FAQ cite, so a regression is a defect. Run it before push/release and on any change touching `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Token usage** — also `scripts/bench.sh`: it measures the recall injection's token cost and hard-fails above the 220-token guard behind the "~180 tokens" promise. The per-prompt token budget is the product's whole pitch, so this is its own gate, not a footnote to performance.
4. **Smoke** — `scripts/smoke.sh` (takes the built binary; runs a throwaway end-to-end pipeline). Proves on the real binary what the unit tests prove in isolation: redaction actually masks stored turns, the encrypted-mirror + restore paths work, the budget override fails closed, and the hooks exit 0. A green unit suite is not a substitute — run this on the shipped binary.
5. **Security review** — `cargo audit` (dependency CVEs, also a CI job) plus `/security-review` over the branch diff. Run before pushing code and always before a release — this repo is public.
6. **Docs in sync** — update every affected markdown file (`README.md`, `docs/*.md`, `CLAUDE.md`, skill docs) in the same change as the behavior it documents. Token/latency claims, flags, and limits in the docs must match the code; a code change that lands without its doc update is incomplete.

Gates 1-4 are `scripts/release-check.sh`; gates 5-6 a script can't run, so do them by hand. Before a public release, run all six green on the final commit, then follow the steps below. Never tag or push a release on an unverified or undocumented tree.

## Releasing

Run `scripts/release-check.sh` green on the exact commit you're about to tag first (gates 1-4 above), and do the `/security-review` + docs-in-sync passes by hand — an unverified tree never gets tagged. Then bump the version in **both** `Cargo.toml` and `.claude-plugin/plugin.json` — they must match. `plugin.json` is the version `/plugin` shows and uses to detect updates, so bumping `Cargo.toml` alone ships a release no installed plugin will pull. Add the new version's section to `CHANGELOG.md` in that same version-bump commit (before the tag, so the tagged tree carries it) — including the `[x.y.z]:` compare-link reference definition at the bottom of the file, or the version heading renders as literal `[x.y.z]` text instead of a link. Then tag `vX.Y.Z` and push the tag — GitHub Actions builds the 4 targets and publishes the release with `sha256sums.txt`. Then pin the new checksums into `hooks/sha256sums.txt` + `hooks/binary-version` (the plugin bootstrap verifies against these), and update the Homebrew formula in `ij5a/homebrew-tap` with the new version + hashes. Last, update the local CLI on PATH to match — `cargo install --git https://github.com/ij5a/subrosa --tag vX.Y.Z --locked --force`, then confirm `subrosa -V`. This step is not optional: `hooks/run.sh` prefers a `subrosa` already on PATH over bootstrapping its own, so a stale PATH binary (e.g. an old `cargo install`) silently keeps the plugin running old code even after `/plugin` shows the new version.
