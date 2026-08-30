# FAQ

## Can my data leave my machine?

subrosa itself uploads no turns, queries, or saved text. It has no telemetry or update checker. It uses 11 direct crates and one static binary. It opens no sockets except the system `curl` child used for the one-time model download, which never uploads turns, queries, or saved text.

The plugin bootstrap downloads the program from GitHub releases. An optional mirror writes one static snapshot to a folder you choose; a sync client may upload that snapshot. The mirror is off by default.

## Where is my data?

| Item | Path | Details |
|---|---|---|
| Live database | `~/.claude/subrosa/memory.db` | Keep it out of synced folders. SQLite WAL and SHM files can corrupt during a write. |
| Snapshots | `~/.claude/subrosa/backups/` | Keeps the last 7 snapshots with owner-only permissions. |
| Mirror | Folder chosen by `subrosa setup` | Holds one static snapshot. With a passphrase, it writes `subrosa-latest.db.enc`. |
| Checkpoint queue | `~/.claude/subrosa/pending-checkpoint.log` | Plain text with one session per line. |
| Semantic model | `~/.claude/subrosa/models/` | About 133 MB, downloaded once and checksum-verified. |
| Index retry state | `~/.claude/subrosa/embed.state` | Appears only after a failed index run and records the next retry time. |

These variables override paths: `SUBROSA_DIR`, `SUBROSA_DB`, `SUBROSA_PROJECTS_DIR`, `SUBROSA_PENDING_LOG`, `SUBROSA_MIRROR`, `SUBROSA_MIRROR_PASSPHRASE`, `SUBROSA_CHECKPOINT_NUDGE`, `SUBROSA_SEMANTIC`, and `SUBROSA_CURL`.

subrosa checks `/usr/bin/curl` and then `/bin/curl`. It ignores `PATH`. Set `SUBROSA_CURL` to an absolute path when `curl` is elsewhere.

## What settings control paths and semantic indexing?

The config file is `~/.claude/subrosa/config`. It uses `KEY=VALUE` lines with mode `0600`.

- `mirror` selects the snapshot folder or `none`.
- `mirror_passphrase` encrypts the mirror when set. Before an encrypted mirror exists, an absent value leaves it readable.
- `checkpoint_nudge` accepts `loud`, `quiet`, or `off`; `loud` is the default.
- `semantic=off` stops automatic indexing, model downloads, and every network call subrosa can make. A model already on disk can still serve semantic searches.

An environment variable wins over its config value. `mirror=none` and `SUBROSA_MIRROR=none` disable mirroring. Remove the setting or run `subrosa setup` to enable it again. An unreadable config counts as `semantic=off`.

## What gets redacted?

Before storage, subrosa masks private-key blocks, AWS access keys, bearer tokens, and `password=` or `token:` values. It masks only the secret part, so the rest stays searchable.

A `passphrase=` line is masked to its end. This also covers quoted text, unquoted text, `export`, and the config file.

Original transcripts stay unchanged under `~/.claude/projects`. Full-disk encryption protects those files at rest.

## What does subrosa not protect?

- Redaction matches known shapes only. A `ghp_...` token, an `sk-...` key, a bare JWT, or another unknown secret stays as written.
- The live database is not encrypted by subrosa. Unix uses `0600` for the file in a `0700` directory. Windows uses its default ACLs.
- Recall can re-inject stored text. A strong match can add up to 3 snippets, and the recall block tells Claude to treat them as unverified.
- Claude Code transcripts remain plaintext under `~/.claude/projects`; subrosa never edits them.
- An optional mirror can leave the machine. A passphrase encrypts that snapshot. Without one, a synced copy is readable. The live database is never synced.

Full-disk encryption protects the live database and local snapshots. It does not change Claude Code's plaintext transcripts.

## How does mirror encryption work?

It protects the mirror only. XChaCha20-Poly1305 encrypts each snapshot with a key from argon2id. The live database, local snapshots, and original transcripts stay unchanged.

The config passphrase is in a `0600` file. A process running as your user can read it. It protects synced-folder and cloud readers, not that user account.

Old plaintext copies in cloud trash or version history remain; remove them yourself. subrosa removes only the exact `subrosa-latest.db` file it wrote, not conflict copies.

Once `subrosa-latest.db.enc` exists, removing the passphrase does not restore plaintext mirroring. Backup reports the missing passphrase, clears the exact plaintext twin, and skips the mirror. To disable encryption, delete the `.enc` file; if iCloud evicted it, delete its hidden `.subrosa-latest.db.enc.icloud` placeholder too.

A fresh salt and nonce each time make every encrypted snapshot a full upload, so useful block reuse is not possible and the upload grows with the database.

A passphrase set outside `subrosa setup` applies at the next backup. The first encrypted snapshot waits for the 24-hour throttle; run `subrosa backup --force` to create it now.

## How does long-term memory work?

Ended sessions enter the checkpoint queue. `/subrosa:checkpoint-backlog` saves durable facts from queued sessions, and `/subrosa:checkpoint` saves facts before `/clear` or `/compact`.

Each fact has a Markdown file and a database row. `subrosa generate` writes `MEMORY.md`. Facts outside the byte budget stay searchable in the archive.

## How many tokens does it cost me?

Recall search and its relevance gate are local. They add `0` tokens and make no model call.

A strong match adds about `180` tokens from up to 3 snippets. A benchmark gate caps the injection at `220` tokens. It injects snippet lines, not full sessions; use `subrosa search` for full text.

`MEMORY.md` loads once per session at up to 23 KB, about 6,000 tokens, by default. Set a per-project budget with `echo 24500 > <memdir>/.budget`; it caps output at about 25,000 bytes or line 200, and extra output is not loaded into context. Saving sessions and deriving tags add `0` tokens.

## Does it get more expensive or slower as the archive grows?

Saving always costs `0` tokens. A strong-match injection is about `180` tokens and always below `220`; a miss adds `0`, whether the archive has 100 sessions or 100,000.

Keyword search uses an FTS5 index. A keyword hit takes about 5 to 11 ms over a 50,000-turn archive. A semantic fallback scans indexed turns linearly, so a miss gets slower as indexed turns grow.

Disk use grows with archived text and semantic vectors. Run [`scripts/bench.sh`](../scripts/bench.sh) to measure latency and recall tokens.

## Performance

These measurements use `scripts/bench.sh`, a synthetic 50,000-turn archive, and an Apple M3 Max. A prompt recall check takes about 4 ms without a match and about 14 ms with one. The full per-prompt hook usually stays under 10 ms on the no-match path, live ingest takes about 7 ms per turn, and archiving 50,000 turns takes about 1.5 seconds.

The static binary is about 5 MB and has no runtime dependencies. Semantic search adds a model file of about 133 MB and one finite background index pass.

## Why do I not see subrosa messages in the chat?

Hook output goes into Claude's context at session start and prompt time. It does not appear as a chat message.

## Recall shows only 3 results. Does it miss things?

Automatic recall returns the top 3 keyword matches. Run `subrosa search -n 20` for a wider ranked list; the archive still contains every saved session.

## Is my current session searchable while it runs?

Yes. The `Stop` hook archives the in-progress transcript after each assistant turn, so it appears in `subrosa search` before the session ends.

The update resumes from a saved byte offset and reads only new lines. It takes about 7 ms per turn and runs after the reply appears. Automatic recall skips the current session because its text is already in context.

## Why keyword search instead of embeddings?

Keyword search is the default. It needs no model, weights, second process, or save-time model call. It matches word roots, so `deploy` finds `deployed`; identifiers such as `TICKET-123` and `my-app-prod` stay exact.

`subrosa search --fuzzy` adds a local trigram index for partial names and one-edit typos. It uses no model.

A plain `subrosa search` retries automatically as semantic search after zero keyword hits when automatic indexing is on and the local model and index are available. `--raw` opts out. It never starts the one-time model download.

Use `subrosa search --semantic` to rank by meaning with the local index. Turns added after the latest index run are omitted, and the command reports how many. The index run that closes the gap starts separately.

The model runs in the same binary. The first automatic index downloads `BAAI/bge-small-en-v1.5` to `~/.claude/subrosa/models/` through system `curl` and checks a pinned sha256. If the model is missing or cannot download, keyword search continues. subrosa redacts the query before embedding it. `bge-small-en-v1.5` is an English model. Non-English text still embeds but generally ranks less well, and keyword search still works for it.

The per-prompt recall injection is keyword-only and always will be. It is not semantic. It runs on every prompt and must stay silent when there is no match. A plain search may use the local semantic index after an exact miss, and `--semantic` uses it directly.

## What are session tags?

subrosa derives tags locally at archive time with no LLM and `0` tokens. Tags include `tool:bash`, `ext:rs`, and `topic:cache-prod`.

Tags are read-only and recomputed from redacted text. Filter with `subrosa sessions --tag tool:kubectl` or `subrosa search deploy --tag topic:aurora`. See one session's tags with `subrosa session <id> --tags`.

## What does the dashboard show?

Bare `subrosa` shows an activity sparkline, database size, per-project share, index budget, and semantic-index progress.

## Proof

Use `scripts/bench.sh` for recall limits and latency. `cargo tree --depth 1` shows 11 direct crates, and `cargo audit` checks vulnerabilities. `src/embed.rs` contains model revisions and sha256 values. Hooks and search make no `connect()` call; the model download is the only system `curl` use. The finite indexer exits when it reaches `ready`.

## How do I verify the binary?

GitHub releases publish `sha256sums.txt`. The plugin and Homebrew formula pin the same hashes. `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Run `/plugin uninstall subrosa@subrosa` to remove the plugin. Delete `~/.claude/subrosa/` and the mirror folder to remove the data. Uninstalling does not touch services, launch agents, or shell profiles.
