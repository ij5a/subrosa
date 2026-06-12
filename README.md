# subrosa

Persistent, private memory for Claude Code. Every session is archived into a local SQLite database and becomes searchable — the work you did last month is one `subrosa search` away, relevant past sessions resurface automatically when you type a related prompt, and Claude stops rediscovering things it already figured out.

## Why "subrosa"?

In ancient Rome, a rose hung over the table meant everything said under it stayed in the room — *sub rosa*, "under the rose." That's the contract this tool makes with your data: every transcript stays on your machine, in a local database, with obvious secret shapes masked before they're stored. No cloud service, no telemetry — your data never leaves your machine. What's said under the rose stays under the rose.

## Quick start

Inside Claude Code, run these two commands:

```
/plugin marketplace add ij5a/subrosa
/plugin install subrosa@subrosa
```

Then start a new Claude Code session (quit and reopen, or run `claude` again). That's the whole install:

- On that first start, the plugin downloads the right prebuilt program for your computer (about 2 MB, checksum-verified against this repo) and archives every Claude Code session already on your disk.
- From then on it runs by itself: each session is archived when it ends, related past sessions are shown to Claude when you type a prompt, and you get a short note when ended sessions are waiting to be saved into long-term memory.

The one-time download fetches the program itself — your data never goes anywhere.

### Optional: the `subrosa` command in your terminal

The plugin works without this. Install the CLI if you also want to search and manage the archive yourself:

```sh
# homebrew
brew install ij5a/tap/subrosa

# or the install script (prebuilt, checksum-verified)
curl -fsSL https://raw.githubusercontent.com/ij5a/subrosa/main/install.sh | sh

# or from source, if you have Rust
cargo install --git https://github.com/ij5a/subrosa
```

The plugin finds a binary on PATH and uses it, so both stay in sync. Two useful first commands:

```sh
subrosa setup    # one-time: pick where backup snapshots mirror to (iCloud / Dropbox / ... / none)
subrosa          # the dashboard
```

## What it does

- **Archives every session.** Hooks ingest each transcript into a local SQLite + FTS5 database when a session ends — quitting Claude Code, running `/clear`, or logging out all count — with a catch-up sweep at session start.
- **Recalls on its own.** When you submit a prompt with enough distinctive terms, the top matching past sessions from the same project are injected into context — quiet unless the match is strong.
- **Builds long-term memory.** Ended sessions queue for checkpointing; the bundled `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` skills distill them into curated facts, and `subrosa generate` renders a byte-budgeted `MEMORY.md` that Claude Code loads every session — it can never overflow.
- **Makes everything searchable.** `subrosa search <terms>` runs ranked full-text search with hyphen-safe handling for identifiers like `my-app-prod` or `TICKET-123`.
- **Shows you the picture.** `subrosa` alone prints the dashboard: activity sparkline, store size, per-project share, index budget.
- **Backs itself up.** Consistent snapshots on a 24h throttle, plus an optional mirror of the latest snapshot to a folder you pick.
- **Masks secrets at the door.** Private key blocks, AWS keys, bearer tokens, and `password=`-style values are redacted before they're written.

## Commands

```sh
subrosa                                  # dashboard (same as: subrosa stats)
subrosa search aurora failover           # find that thing from three weeks ago
subrosa search --project api deploy      # scope to one project
subrosa search -n 30 --raw 'cache OR redis'

subrosa fact list                        # curated facts for the current project
subrosa fact upsert --leaf note.md       # add/update a fact from a leaf file
subrosa generate                         # rebuild MEMORY.md (byte-budgeted)
subrosa import ~/.claude/projects/<project>/memory   # one-time import of an existing MEMORY.md

subrosa session <id>                     # dump one archived session
subrosa pending                          # sessions queued for checkpointing
subrosa checkpoint-drop <id>             # de-queue one session after saving it

subrosa sweep                            # catch up on changed transcripts
subrosa backup --force                   # snapshot now
subrosa setup                            # one-time backup-mirror question
```

## The memory workflow

1. A session ends (you quit Claude Code, run `/clear`, or log out) → it's archived and queued.
2. Next session start, you see: `[subrosa] N session(s) queued for checkpoint…`
3. Run `/subrosa:checkpoint-backlog` — Claude reads each queued session and saves the durable facts into that project's memory (leaf files + the facts table), then regenerates `MEMORY.md`.
4. Before `/clear` or `/compact`, run `/subrosa:checkpoint` to do the same for the live session.

`MEMORY.md` is generated from the facts table under a byte budget — important facts (pinned, feedback) win when space runs out, and everything that doesn't fit stays searchable in the archive.

## Where your data lives

| What | Where | Why |
|---|---|---|
| Live database | `~/.claude/subrosa/memory.db` | Stays out of synced folders — cloud sync corrupts live SQLite (WAL/SHM sidecars) |
| Snapshots | `~/.claude/subrosa/backups/` | Last 7 kept, owner-only permissions |
| Mirror | the folder you picked in `subrosa setup` | A single static snapshot file is safe to sync |
| Checkpoint queue | `~/.claude/subrosa/pending-checkpoint.log` | Plain text, one session per line |

Everything is overridable with env vars: `SUBROSA_DIR`, `SUBROSA_DB`, `SUBROSA_PROJECTS_DIR`, `SUBROSA_PENDING_LOG`, `SUBROSA_MIRROR`.

## Privacy model

- Local-only. The binary makes zero network calls; recall reads only your local database, and hook output goes only into your own session.
- Recalled snippets are your own archived text resurfaced verbatim — the injection block tells Claude to verify before relying on them.
- The database is `0600`, its directory `0700`.
- Secret shapes are redacted before storage. The source transcripts under `~/.claude/projects` remain as Claude Code wrote them — full-disk encryption is the real at-rest control for those.

## Performance

A hook that runs on every prompt has to be invisible. Measured with `scripts/bench.sh`
(hyperfine, synthetic 50,000-turn / 28 MB archive, Apple M3 Max):

| Operation | Time |
|---|---|
| Prompt recall check, no match — the usual case | ~4 ms |
| Prompt recall check, match found and injected | ~14 ms |
| A full hook fire as Claude Code runs it (shell wrapper + binary) | under 10 ms |
| Session-start catch-up sweep, nothing changed | ~5 ms |
| `subrosa search` over 50,000 turns | 5–11 ms |
| Archiving 50,000 turns from scratch (first install) | ~1.1 s |

One static 3.7 MB binary, no background process, no runtime dependencies — every hook
is a short-lived process that opens the database, does its work, and exits. Reproduce
the numbers with `scripts/bench.sh` (needs `hyperfine`).

## Development

```sh
mise install                          # pinned Rust toolchain (mise.lock)
git config core.hooksPath .githooks   # once per clone: sweep + fmt + clippy + tests on commit
cargo build && cargo test
```

The output formats (stored text, session dump, `MEMORY.md`, recall block) are pinned byte-for-byte by the golden tests in `tests/` — a failing golden test means a format change that needs a deliberate decision. Point everything at a throwaway dir so you never touch your live data:

```sh
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
```

## License

MIT
