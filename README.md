<p align="center">
  <img src="assets/subrosa-wordmark-animated.svg" alt="subrosa" width="100%">
</p>

<p align="center">
  Persistent, private memory for Claude Code — that never spends your tokens on itself.
</p>

<p align="center">
  <a href="https://github.com/ij5a/subrosa/releases/latest"><img src="https://img.shields.io/github/v/release/ij5a/subrosa" alt="latest release"></a>
  <a href="https://github.com/ij5a/subrosa/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ij5a/subrosa/ci.yml?branch=main" alt="CI status"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/ij5a/subrosa" alt="MIT license"></a>
</p>

Every session is archived into a local SQLite database and made searchable: last month's work is one `subrosa search` away, relevant past sessions resurface when you type a related prompt, and Claude stops rediscovering what it already figured out. Pulling up an old answer costs a few hundred tokens; having Claude re-derive it from scratch runs into the thousands.

- **No LLM calls to save memory.** Saving a session is plain-text parsing — zero tokens. Most memory plugins run your sessions through an LLM to save them ([the comparison](docs/comparison.md) has the numbers, from their own docs).
- **Hard token limits, set in the code.** Recall adds ~180 tokens on a strong match, usually nothing — it stays silent otherwise. The always-loaded index is capped at 23 KB by default. [Check it yourself](#proof-verify-it-yourself), or see [where your tokens go](docs/faq.md#how-many-tokens-does-it-cost-me).
- **One ~5 MB static binary.** No daemon, no worker port, no background process — a per-prompt hook fires, finishes in under 10 ms, and exits. (The daily backup at session end takes a second or two when mirror encryption is on.)
- **Your transcripts stay on your machine.** No cloud, no telemetry, and obvious secret shapes are masked before storage. The binary opens no sockets; the one opt-in feature that needs the internet at all (`search --semantic`) downloads its model once and then runs it on your machine. [Verify every claim yourself](#proof-verify-it-yourself).

<p align="center">
  <img src="assets/demo.gif" alt="subrosa demo: search the archive, automatic recall on a prompt, dashboard" width="800">
</p>

<p align="center">
  <a href="#why-subrosa">Why "subrosa"?</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#what-it-does">What it does</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#the-memory-workflow">The memory workflow</a> ·
  <a href="#make-claude-use-the-archive-itself">Make Claude use the archive itself</a><br/>
  <a href="#where-your-data-lives">Where your data lives</a> ·
  <a href="#privacy-model">Privacy model</a> ·
  <a href="#proof-verify-it-yourself">Proof</a> ·
  <a href="#performance">Performance</a> ·
  <a href="#development">Development</a> ·
  <a href="docs/comparison.md">How it compares</a> ·
  <a href="docs/faq.md">FAQ</a>
</p>

## Why "subrosa"?

In ancient Rome, a rose over the table meant everything said under it stayed in the room — *sub rosa*. That's the contract here: every transcript stays on your machine, in a local database, secret shapes masked before storage. No cloud, no telemetry.

## Quick start

Inside Claude Code, run these two commands:

```
/plugin marketplace add ij5a/subrosa
/plugin install subrosa@subrosa
```

Then start a new Claude Code session (quit and reopen, or run `claude` again). That's the whole install:

- On first start, the plugin downloads the right prebuilt program for your computer (~2.5 MB, checksum-verified against this repo) and archives every session already on your disk.
- After that it runs itself: archives each session as you work and again when it ends, shows Claude related past sessions when you prompt, and notes when ended sessions are waiting to be saved into long-term memory.

The one-time download fetches the program only — your data never moves.

One more command, for the best experience — it teaches Claude to work the archive on its own: search it at the start of any task, and clear the checkpoint backlog when sessions queue up ([what it adds](#make-claude-use-the-archive-itself); safe to re-run, ~250 tokens):

```sh
~/.claude/subrosa/bin/subrosa init --claude-md   # or just: subrosa init --claude-md, with the CLI below
```

### Optional: the `subrosa` command in your terminal

The plugin works without this. Install the CLI to also search and manage the archive yourself:

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

- **Archives every session.** When one ends — quitting Claude Code, `/clear`, or logging out — its transcript is saved to a local SQLite (full-text search) database, with a catch-up sweep at the next start.
- **Recalls on its own.** A prompt with enough distinctive terms pulls the top matching past sessions from the same project into context — silent unless the match is strong.
- **Builds long-term memory.** Ended sessions queue up; `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` distill them into curated facts (one per small markdown file). `subrosa generate` then renders `MEMORY.md` — a short, size-capped index Claude Code loads every session, and `subrosa fact search` finds a specific fact once a project has dozens.
- **Makes everything searchable.** `subrosa search <terms>` runs ranked full-text search — identifiers like `my-app-prod` or `TICKET-123` stay exact. Narrow with `--project`/`--after`/`--before`/`--tag`, read around a hit with `-C/--context`, filter with `--exclude` (drop a term) and `--any` (match any term, not all), or fall back to `--fuzzy` for partial names and small typos.
- **Lists and filters sessions.** `subrosa sessions` shows past sessions newest-first, filterable by `--project`, date (`--after`/`--before`), and `--tag`. Each session carries auto-derived tags (`tool:bash`, `ext:rs`, `topic:cache-prod`) — computed locally at archive time, so they cost zero tokens and nothing to maintain.
- **Shows what clusters together.** `subrosa related <identifier>` ranks the terms and sessions that recur alongside something like `auth.ts` or `TICKET-123` — read from the archive, not guessed. It answers "what did this work touch," which `search` can't.
- **Follows your curated links.** Notes link to each other with `[[name]]`; `subrosa fact link <slug>` shows what a note links to and what links back, and flags dead links.
- **Catches broken notes.** `subrosa fact doctor` lints a project's memory folder and names what's wrong: frontmatter that's missing, unclosed, or spliced (the shape that leaves a note on disk that quietly stops loading), duplicate name slugs, dead links, and facts pointing at files that are gone. Read-only — it reports the fix, it never edits a note.
- **Shows you the picture.** `subrosa` alone prints the dashboard: activity sparkline, store size, per-project share, index budget.
- **Backs itself up.** Consistent snapshots on a 24h throttle, plus an optional mirror of the latest to a folder you pick. Set a passphrase and the mirror copy is encrypted (XChaCha20-Poly1305, key from argon2id); `subrosa restore` reads it back.
- **Masks secrets at the door.** Private key blocks, AWS keys, bearer tokens, and `password=`-style values are redacted before they're written. A `passphrase=` line is masked all the way to the end of that line, because a passphrase can contain spaces.

## Commands

```sh
subrosa                                  # dashboard (same as: subrosa stats)
subrosa search aurora failover           # find that thing from three weeks ago
subrosa search --project api deploy      # scope to one project
subrosa search -n 30 --raw 'cache OR redis'
subrosa search --fuzzy ratelimiter       # substring + small-typo matching (builds a trigram index on first use)
subrosa search deploy --after 2026-05-01 # only turns from May onward (--before too; YYYY-MM-DD, inclusive)
subrosa search api --tag tool:kubectl    # only sessions that used a given tool (repeat --tag to AND)
subrosa search pgbouncer -C 2            # print 2 turns on each side of every hit (read around the match)
subrosa search timeout --exclude test    # drop hits that also mention a term (repeat --exclude to add more)
subrosa search redis valkey --any        # match any of the terms (OR) instead of all (AND)
subrosa embed                            # one-time: precompute embeddings (opt-in; downloads a ~133 MB model on first run)
subrosa embed --rebuild                  # drop the stored vectors and embed everything again
subrosa search --semantic 'why did checkout get slow'   # rank by meaning, no shared word needed
subrosa related cache-prod               # terms + sessions that co-occur with an identifier
subrosa related TICKET-123 --project api # what clustered around it, scoped to one project

subrosa sessions                         # past sessions, newest first, with their tags
subrosa sessions --tag topic:cache-prod --after 2026-05-01   # filter by tag and/or date
subrosa session <id> --tags              # dump a session and show its auto-derived tags

subrosa fact list                        # curated facts for the current project
subrosa fact search pgbouncer            # full-text search the curated facts (bm25-ranked)
subrosa fact upsert --leaf note.md       # add/update a fact from a markdown file (one fact per file)
subrosa fact link auth-decision          # show [[name]] links into/out of a fact (flags dead links)
subrosa fact doctor                      # lint the notes + facts of one project (read-only; exit 1 on a break)
subrosa generate                         # rebuild MEMORY.md (byte-budgeted)
subrosa import ~/.claude/projects/<project>/memory   # one-time import of an existing MEMORY.md

subrosa session <id>                     # dump one archived session (full id or unique prefix)
subrosa pending                          # sessions queued for checkpointing
subrosa checkpoint-drop <id>             # de-queue one session after saving it

subrosa sweep                            # catch up on changed transcripts
subrosa backup --force                   # snapshot now
subrosa restore <mirror>/subrosa-latest.db.enc   # decrypt an encrypted mirror snapshot
subrosa setup                            # one-time backup-mirror question
```

## The memory workflow

```mermaid
flowchart TD
    ended["session ends<br/>quit, /clear, or log out"] --> archived["archived into the local database<br/>+ queued for checkpoint"]
    archived --> nudge["next session start<br/>an action-required note lands in Claude's context:<br/>N sessions queued for checkpoint"]
    nudge --> backlog["/subrosa:checkpoint-backlog<br/>saves durable facts from each queued session"]
    livesession["live session<br/>before /clear or /compact"] --> checkpoint["/subrosa:checkpoint<br/>saves durable facts from the current session"]
    backlog --> facts["curated facts<br/>one file per fact + the facts database"]
    checkpoint --> facts
    facts -->|subrosa generate| memorymd["MEMORY.md<br/>byte-budgeted, loaded every session"]
    archived -.->|stays searchable either way| recall["subrosa search + automatic prompt recall"]
```

1. A session ends → it's archived and queued.
2. At the next start, an `ACTION REQUIRED` note lands in Claude's context (`[subrosa] ACTION REQUIRED — N session(s) queued for checkpoint…`) — hook output goes to Claude, not your chat window, so Claude acts on it (or you run the next step yourself). The same reminder then rides each prompt until the queue clears, so a busy first task can't bury it. Prefer the calmer one-liner? Set `checkpoint_nudge=quiet` (or `off`) — see [Where your data lives](#where-your-data-lives).
3. Run `/subrosa:checkpoint-backlog` — Claude saves the important facts from each queued session into that project's memory, then rebuilds `MEMORY.md`.
4. Before `/clear` or `/compact`, run `/subrosa:checkpoint` to do the same for the live session.

`MEMORY.md` is built under a byte budget — important facts (pinned, feedback) win when space runs out, and everything that doesn't fit stays searchable in the archive.

## Make Claude use the archive itself

Recall only fires when you type a prompt. To have Claude work the archive on its own, add these standing instructions to your `CLAUDE.md` (`~/.claude/CLAUDE.md` covers every project, a repo's own covers just that one) — or run `subrosa init --claude-md` to append them for you (idempotent, and it adds only the sections you're missing).

The first has Claude search the archive at the start of any task:

```markdown
## Memory recall (subrosa)

Every past Claude Code session is archived locally and searchable with
`subrosa search "<keywords>"` — scope with `--project <name>`, narrow by date or
tag with `--after`/`--before`/`--tag`, more results with `-n 20`, and retry with
`--fuzzy` if an exact search finds nothing (partial names, small typos).
(If `subrosa` isn't on PATH, it's at `~/.claude/subrosa/bin/subrosa`.)
At the start of any task — investigating, debugging, designing, reviewing, or when
a ticket, environment, resource, person, or past decision comes up — search the
archive first and build on what past sessions already worked out instead of
starting cold. Announce the search ("Searching past sessions for [topic]...") and
cite hits with their date. Skip only for trivial one-liners. `MEMORY.md` is
generated — never hand-edit it; update facts with `subrosa fact` + `subrosa generate`,
or run `/subrosa:checkpoint`.
```

The second has Claude clear the checkpoint backlog in the background when sessions queue up:

```markdown
## Memory auto-checkpoint (subrosa)

When a `[subrosa] ACTION REQUIRED` note says sessions are queued for checkpoint
(or `subrosa pending` is non-empty), run the `/subrosa:checkpoint-backlog` skill
in the background — never before or blocking the task you're working on. It saves
the durable facts from each queued session into that project's memory, then clears
the queue as it finishes. Skip it silently when nothing is queued.
```

Together they add about 250 tokens of always-loaded context — your call. The first makes Claude write its own searches mid-task; the second keeps ended sessions from piling up unsaved.

## Where your data lives

| What | Where | Why |
|---|---|---|
| Live database | `~/.claude/subrosa/memory.db` | Not in synced folders — cloud sync corrupts a live SQLite database (its WAL/SHM helper files) |
| Snapshots | `~/.claude/subrosa/backups/` | Last 7 kept, owner-only permissions |
| Mirror | the folder you picked in `subrosa setup` | A single static snapshot file is safe to sync; encrypted as `subrosa-latest.db.enc` when you set a passphrase |
| Checkpoint queue | `~/.claude/subrosa/pending-checkpoint.log` | Plain text, one session per line |
| Embedding model | `~/.claude/subrosa/models/` | Only if you run `subrosa embed`; ~133 MB, downloaded once and checksum-verified |

Everything is overridable with env vars: `SUBROSA_DIR`, `SUBROSA_DB`, `SUBROSA_PROJECTS_DIR`, `SUBROSA_PENDING_LOG`, `SUBROSA_MIRROR`, `SUBROSA_MIRROR_PASSPHRASE`, `SUBROSA_CHECKPOINT_NUDGE`.

Three settings also live in `~/.claude/subrosa/config` (plain `KEY=VALUE`, mode `0600`): `mirror` (the snapshot folder, or `none`), `mirror_passphrase` (set it and the mirror copy is encrypted; leave it out and the mirror stays plaintext), and `checkpoint_nudge` — `loud` (default, the `ACTION REQUIRED` block), `quiet` (a one-line reminder), or `off`. The matching env var wins when set, with one exception: `none` turns mirroring off whichever side says it, so `mirror=none` in the config also switches off a `SUBROSA_MIRROR` override — and `SUBROSA_MIRROR=none` switches off a configured folder. Delete the line, unset the variable, or re-run `subrosa setup`, to mirror again. Turning it off only stops subrosa writing there: if the other side still names a real folder, `subrosa restore` goes on refusing to put a readable copy into it.

## Privacy model

- **Local-only.** Recall reads only your local database, and hook output goes only into your own session. No hook, no ingest and no recall ever opens a socket.
- **Locked down.** The database and its folder are readable only by you (`0600`/`0700`), and secret shapes are redacted before storage.
- **One opt-in exit, yours.** A backup-mirror snapshot leaves the machine only if you point it at a synced folder (off by default; set a mirror passphrase and that copy goes out encrypted). The live database is never synced.
- **Nothing you typed is ever uploaded.** The one time subrosa reaches the internet is the model download for `subrosa embed`, which only pulls files down. Your turns and your queries are embedded on your own machine, by a model on your own disk.

Full limits — what redaction misses, why the archive isn't encrypted, what recall re-injects — are in the [FAQ](docs/faq.md#what-does-subrosa-not-protect).

## Proof: verify it yourself

Claims are only worth the commands that check them. The whole thing is ~9,000 lines of Rust, MIT licensed.

| Claim | Check it | What you'll see |
|---|---|---|
| Token limits are constants in the code | read `MAX_INJECT` + `SNIPPET_CHARS` in `src/recall.rs`, `DEFAULT_BUDGET` in `src/generate.rs` | `MAX_INJECT = 3`, `SNIPPET_CHARS = 160` (≈ 180 tokens at recall), and a 23 KB index budget — values you can read, not settings that drift. Raise it per project with `echo 24500 > <memdir>/.budget` (Claude stops reading past ~25 KB / line 200) |
| Recall stays near ~180 tokens a prompt | `scripts/bench.sh` — the `recall injection` line | the injected block weighed in bytes → ~180 tokens on a strong match (3 snippets) |
| No networking library in the build | `cargo tree -e normal \| grep -Ei 'reqwest\|hyper\|tokio\|rustls\|openssl\|curl'` | no output — no HTTP or networking library at all. Trace a hook or a search with `strace`/`dtruss` and see no `connect()`. The one-time model download runs your own `curl`, as a separate process you can watch |
| The embedding model is pinned | read `FILES` + `REVISION` in `src/embed.rs` | one revision of `BAAI/bge-small-en-v1.5` and a sha256 per file; a download that doesn't match is deleted, not loaded |
| The supply chain is small and audited | `cargo tree --depth 1` · `cargo audit` | 11 direct dependencies, no known advisories; CI runs `cargo audit` on every push, and release binaries ship a pinned `sha256sums.txt` |

More checks (redaction, file permissions, no background process) and the honest limits are in the [FAQ](docs/faq.md#what-does-subrosa-not-protect).

## Performance

A hook that runs on every prompt has to be invisible. Measured with `scripts/bench.sh` (hyperfine, synthetic 50,000-turn / 28 MB archive, Apple M3 Max):

| Operation | Time |
|---|---|
| Prompt recall check, no match — the usual case | ~4 ms |
| Prompt recall check, match found and injected | ~14 ms |
| A full per-prompt hook fire as Claude Code runs it (shell wrapper + binary) | under 10 ms |
| Session-end backup, once per 24h, with mirror encryption on | a second or two (argon2id + sealing the whole snapshot) |
| Session-start catch-up sweep, nothing changed | ~5 ms |
| Live-session ingest after a turn (one transcript, flat at any session length) | ~7 ms |
| `subrosa search` over 50,000 turns | 5–11 ms |
| `subrosa related <identifier>` over 50,000 turns | 0.3–0.4 s |
| Archiving 50,000 turns from scratch (first install) | ~1.5 s |

One static ~5 MB binary, no background process, no runtime dependencies. (Opt-in semantic search adds a ~133 MB model file on disk — nothing else changes.)

## Development

```sh
mise install                          # pinned Rust toolchain (mise.lock)
git config core.hooksPath .githooks   # once per clone: sweep + fmt + clippy + tests on commit
cargo build && cargo test
```

The output formats (stored text, session dump, `MEMORY.md`, recall block) are pinned byte-for-byte by the golden tests in `tests/` — a failing golden means a deliberate format change. Point everything at a throwaway dir so you never touch live data:

```sh
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
```

## License

MIT
