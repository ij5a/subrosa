# FAQ

## Can data leave my machine?

subrosa uploads no turns, queries, or saved text. It has no telemetry or update checker. It uses 11 direct crates, one static binary, and only the system `curl` child for the one-time model download. The download uploads no turns, queries, or saved text.

The plugin bootstrap downloads the program from GitHub releases. An optional mirror writes one static snapshot to your folder. A sync client may upload it. The mirror is off by default.

## Where is data stored?

| Item | Path | Details |
|---|---|---|
| Live database | `~/.claude/subrosa/memory.db` | Keep it out of synced folders. SQLite WAL and SHM files can corrupt during a write. |
| Snapshots | `~/.claude/subrosa/backups/` | Keeps the last 7 snapshots with owner-only permissions. |
| Mirror | Folder chosen by `subrosa setup` | Holds one static snapshot. With a passphrase, it writes `subrosa-latest.db.enc`. |
| Checkpoint queue | `~/.claude/subrosa/memory.db` | SQLite table. Older versions used `~/.claude/subrosa/pending-checkpoint.log`; upgrade imports it once and leaves it untouched. |
| Semantic model | `~/.claude/subrosa/models/` | About 133 MB, downloaded once and checksum-verified. |
| Index retry state | `~/.claude/subrosa/embed.state` | Records the next retry time when present. Healthy runs can also leave it present. |

These variables control data paths, secrets, modes, and tools: `SUBROSA_DIR`, `SUBROSA_DB`, `SUBROSA_PROJECTS_DIR`, `SUBROSA_PENDING_LOG`, `SUBROSA_MIRROR`, `SUBROSA_MIRROR_PASSPHRASE`, `SUBROSA_CHECKPOINT_NUDGE`, `SUBROSA_SEMANTIC`, and `SUBROSA_CURL`. The legacy import alone reads `SUBROSA_PENDING_LOG`.

subrosa checks `/usr/bin/curl` and then `/bin/curl`. It ignores `PATH`. Set `SUBROSA_CURL` to an absolute path when `curl` is elsewhere.

If a transcript is rewritten instead of appended to, subrosa refuses it, keeps what it already archived, and leaves the session queued. After checking the file, use the documented SQLite repair to clear the stored incomplete state and reset the cursor, then run `subrosa ingest --require-complete` again.

## What settings control paths and index?

The config file is `~/.claude/subrosa/config`. It uses `KEY=VALUE` lines with mode `0600`.

- `mirror` selects the snapshot folder or `none`.
- `mirror_passphrase` encrypts the mirror when set. Before an encrypted mirror exists, an absent value leaves it readable.
- `checkpoint_nudge` accepts `loud`, `quiet`, or `off`; `loud` is the default.
- `semantic=off` stops automatic indexing, model downloads, and every network call subrosa can make. A model already on disk can still serve semantic searches.

An environment variable usually wins over config. `mirror=none` is the exception: it always disables mirroring, even when `SUBROSA_MIRROR` names a folder. `SUBROSA_MIRROR=none` also disables mirroring. For semantic indexing, `SUBROSA_SEMANTIC` wins over config, so it can override `semantic=off`. Remove the setting or run `subrosa setup` to enable mirroring. An unreadable config means `semantic=off`.

## What gets redacted?

Before storage, subrosa masks private-key blocks, AWS access keys, bearer tokens, and `password=` or `token:` values. Only the secret part is masked, so the rest stays searchable.

A `passphrase=` line is masked to its end. This covers quoted or unquoted text, `export`, and the config file.

Original transcripts stay unchanged under `~/.claude/projects`. Full-disk encryption protects them.

## What does subrosa not protect?

- Redaction matches known shapes only. A `ghp_...` token, an `sk-...` key, a bare JWT, or another unknown secret stays as written.
- The live database is not encrypted. Unix uses `0600` in a `0700` directory. Windows uses its default ACLs.
- Recall can re-inject stored text. A strong match can add up to 3 snippets, and the recall block tells Claude to treat them as unverified.
- Claude Code transcripts remain plaintext under `~/.claude/projects`; subrosa never edits them.
- An optional mirror can leave the machine. A passphrase encrypts that snapshot. Without one, a synced copy is readable. The live database is never synced.

## How does mirror encryption work?

It protects only the mirror. XChaCha20-Poly1305 encrypts each snapshot with an argon2id key. The live database, local snapshots, and original transcripts stay unchanged.

The config passphrase is in a `0600` file. Your user can read it. It protects synced-folder readers, not that account.

Old plaintext copies in cloud trash or history remain; remove them. subrosa removes only its exact `subrosa-latest.db`, not conflict copies.

Once `subrosa-latest.db.enc` exists, removing the passphrase does not restore plaintext mirroring. Backup reports the missing passphrase, clears the plaintext twin, and skips the mirror. Disable encryption by deleting `.enc`; if iCloud evicted it, delete `.subrosa-latest.db.enc.icloud` too.

A fresh salt and nonce make every encrypted snapshot a full upload. Block reuse is impossible. Upload size grows with the database.

A passphrase set outside `subrosa setup` applies at the next backup. The first encrypted snapshot waits for the 24-hour throttle. Run `subrosa backup --force` to create it now.

## How does long-term memory work?

Ended sessions start a detached worker and return. It archives the transcript, adds it to the SQLite queue, retries database contention for a bounded time, and lets a later sweep recover missed work. `/subrosa:checkpoint-backlog` saves queued facts. `/subrosa:checkpoint` saves facts before `/clear` or `/compact`.

Each fact has a Markdown file and database row. `subrosa generate` writes `MEMORY.md`. Facts outside the byte budget stay searchable.

## How many tokens does it cost?

Recall search and its relevance gate are local. They add `0` tokens and make no model call.

A strong match adds about `180` estimated tokens from up to 3 snippets. The benchmark uses response bytes divided by `3.8` as an estimate from one fixture; it is not a runtime token cap. Unicode text can exceed `220` by that estimate. It injects snippet lines, not full sessions; use `subrosa search` for full text.

`MEMORY.md` loads once per session at up to 23 KB by default, about 6,000 tokens. Set a per-project budget with `echo 24500 > <memdir>/.budget`; it caps output at about 25,000 bytes or line 200. Extra output is not loaded. Saving and tag derivation add `0` tokens.

## Does it slow as the archive grows?

Saving always costs `0` tokens. A strong match adds about `180` estimated tokens in the benchmark fixture; a miss adds `0`, whether the archive has 100 sessions or 100,000.

Keyword search uses an FTS5 index. A keyword hit takes about 5 to 11 ms over a 50,000-turn archive. A semantic fallback scans indexed turns linearly, so a miss gets slower as indexed turns grow.

Disk use grows with archived text and semantic vectors. Run [`scripts/bench.sh`](../scripts/bench.sh) to measure latency and recall tokens.

## Performance

These measurements use `scripts/bench.sh`, a synthetic 50,000-turn archive, and an Apple M3 Max. Recall takes about 4 ms without a match or 14 ms with one. The full hook usually stays under 10 ms without a match. Live ingest takes about 7 ms per turn. Archiving 50,000 turns takes about 1.5 seconds.

The static binary is about 5 MB with no runtime dependencies. Semantic search adds a 133 MB model and one finite background index pass.

## Why no messages in chat?

Hook output goes into context at session start and prompt time. It does not appear as a chat message.

## Recall shows 3 results. Does it miss things?

Automatic recall returns the top 3 keyword matches. Run `subrosa search -n 20 cache` for a wider ranked list; the archive still contains every saved session.

## Is the current session searchable?

Yes. The `Stop` hook archives the in-progress transcript after each assistant turn, so it appears in `subrosa search` before the session ends.

The update resumes from a saved byte offset and reads only new lines. It takes about 7 ms per turn and runs after the reply appears. Automatic recall skips the current session because its text is already in context.

## Why keyword search?

Keyword search is the default. It needs no model, weights, second process, or save-time model call. It matches word roots, so `deploy` finds `deployed`; identifiers such as `TICKET-123` and `my-app-prod` stay exact.

`subrosa search --fuzzy` adds a local trigram index for partial names and one-edit typos. It uses no model.

A plain `subrosa search` retries semantic search after zero keyword hits when automatic indexing is on and the local model and index are ready. `--raw` opts out. It never starts the one-time model download.

Use `subrosa search --semantic` to rank with the local index. Turns added after the latest index run are omitted and counted. A separate index run closes the gap.

The model runs in the binary. The first automatic index downloads `BAAI/bge-small-en-v1.5` to `~/.claude/subrosa/models/` through system `curl` and checks pinned sha256. If it is missing or cannot download, keyword search continues. subrosa redacts the query before embedding. `bge-small-en-v1.5` is English. Non-English text still embeds but usually ranks worse; keyword search still works.

Per-prompt recall is always keyword-only. It runs on every prompt and stays silent without a match. A plain search may use the local semantic index after an exact miss; `--semantic` uses it directly.

## What are session tags?

subrosa derives tags locally at archive time with no model call and `0` tokens. Tags include `tool:bash`, `ext:rs`, and `topic:cache-prod`.

Tags are read-only and recomputed from redacted text. Filter with `subrosa sessions --tag tool:kubectl` or `subrosa search deploy --tag topic:aurora`. See one session's tags with `subrosa session <id> --tags`.

## What does the dashboard show?

Bare `subrosa` shows an activity sparkline, database size, project share, index budget, and semantic-index progress.

## Proof

Use `scripts/bench.sh` for recall limits and latency. `cargo tree --depth 1` shows 11 direct crates; `cargo audit` checks vulnerabilities. `src/embed.rs` contains model revisions and sha256 values. Hooks and search make no `connect()` call. The model download is the only system `curl` use. The indexer exits at `ready`.

## How do I verify the binary?

GitHub releases publish `sha256sums.txt`. The plugin and Homebrew formula pin the same hashes. `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Run `/plugin uninstall subrosa@subrosa` to remove the plugin. Delete `~/.claude/subrosa/` and the mirror folder. Uninstalling does not touch services, launch agents, or shell profiles.
