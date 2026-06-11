# subrosa

Persistent, private memory for Claude Code. Every session is archived into a local SQLite database and becomes searchable — the work you did last month is one `subrosa search` away, and Claude stops rediscovering things it already figured out.

## Why "subrosa"?

In ancient Rome, a rose hung over the table meant everything said under it stayed in the room — *sub rosa*, "under the rose." That's the contract this tool makes with your data: every transcript stays on your machine, in a local database, with obvious secret shapes masked before they're stored. No cloud service, no telemetry, no network calls — the binary never talks to the internet. What's said under the rose stays under the rose.

## What it does

- **Archives every session.** Claude Code hooks ingest each transcript into a local SQLite + FTS5 database when a session ends, with a catch-up sweep at session start.
- **Makes it searchable.** `subrosa search <terms>` runs ranked full-text search over everything you and Claude ever said, with hyphen-safe handling for identifiers like `my-app-prod` or `TICKET-123`.
- **Backs itself up.** Consistent snapshots on a 24h throttle, plus an optional mirror of the latest snapshot to a folder you pick — iCloud, Dropbox, Google Drive, OneDrive, a custom path, or none. `subrosa setup` asks once.
- **Masks secrets at the door.** Private key blocks, AWS keys, bearer tokens, and `password=`-style values are redacted before they're written to the archive.

## Status

Early (v0.1.0). Archive, search, backups, and the hook wiring work, and ingestion is parity-tested against the proven implementation this is a rewrite of — same transcript in, byte-identical archive out. On the roadmap:

- Recall hook: inject relevant past sessions into context when you submit a prompt
- Curated facts and a generated, byte-budgeted `MEMORY.md`
- Checkpoint skills to distill ended sessions into durable memory
- Stats dashboard
- Prebuilt binaries and Windows support

## Install

Two pieces: the binary (does the work) and the plugin (wires the hooks).

**1. The binary** (needs a Rust toolchain until prebuilt releases land):

```sh
cargo install --git https://github.com/ij5a/subrosa
```

**2. The plugin** (inside Claude Code):

```
/plugin marketplace add ij5a/subrosa
/plugin install subrosa@subrosa
```

**3. One-time setup** — creates the database and asks where backup snapshots should mirror to (iCloud / Dropbox / Google Drive / OneDrive / custom / none):

```sh
subrosa setup
subrosa ingest --sweep   # archive every session you already have
```

From then on the hooks work invisibly: SessionEnd archives the session you just finished, SessionStart catches up anything missed, and a snapshot is taken at most once a day.

## Use

```sh
subrosa search aurora failover              # find that thing from three weeks ago
subrosa search --project api deploy         # scope to one project
subrosa search -n 30 --raw 'cache OR redis' # raw FTS5 syntax
subrosa ingest --sweep                      # catch up manually
subrosa backup --force                      # snapshot now
subrosa pending                             # sessions queued for checkpointing
```

## Where your data lives

| What | Where | Why |
|---|---|---|
| Live database | `~/.claude/subrosa/memory.db` | Stays out of synced folders — cloud sync corrupts live SQLite (WAL/SHM sidecars) |
| Snapshots | `~/.claude/subrosa/backups/` | Last 7 kept, owner-only permissions |
| Mirror | the folder you picked in `subrosa setup` | A single static snapshot file is safe to sync |

Everything is overridable with env vars: `SUBROSA_DIR`, `SUBROSA_DB`, `SUBROSA_PROJECTS_DIR`, `SUBROSA_MIRROR`.

## Privacy model

- Local-only. The binary makes zero network calls.
- The database is `0600`, its directory `0700`.
- Secret shapes are redacted before storage. The source transcripts under `~/.claude/projects` remain as Claude Code wrote them — full-disk encryption is the real at-rest control for those.

## Development

```sh
mise install                    # pinned Rust toolchain (mise.lock)
cargo build && cargo test
cargo clippy --all-targets && cargo fmt --check
```

Point everything at a throwaway dir so you never touch your live data:

```sh
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
```

## License

MIT
