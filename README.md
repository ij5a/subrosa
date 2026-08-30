<p align="center">
  <img src="assets/subrosa-wordmark-animated.svg" alt="subrosa" width="100%">
</p>

<p align="center">
  Persistent local memory for Claude Code.
</p>

<p align="center">
  <a href="https://github.com/ij5a/subrosa/releases/latest"><img src="https://img.shields.io/github/v/release/ij5a/subrosa" alt="latest release"></a>
  <a href="https://github.com/ij5a/subrosa/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ij5a/subrosa/ci.yml?branch=main" alt="CI status"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/ij5a/subrosa" alt="MIT license"></a>
</p>

subrosa archives every Claude Code session in a local SQLite database. Search the archive with `subrosa search`.

A plain `subrosa search` retries semantic search after zero keyword hits when automatic indexing is on and its local model and index are available. Use `--raw` to skip that retry. Plain search never starts the one-time model download.

Prompt recall is keyword-only. It adds up to 3 relevant snippets when the match is strong.

- Saving a session uses plain-text parsing. It makes no LLM call and uses 0 model tokens.
- The project has 11 direct crates and one static binary of about 5 MB. The binary has no daemon or worker.
- The binary opens no sockets. A system `curl` child makes the one-time model download.
- Nothing you type, save, or search is uploaded. Secret shapes are masked before storage.
- Recall costs about 180 tokens on a strong match and stays below the 220-token benchmark limit. `MEMORY.md` uses at most 23 KB by default.
- Keyword hits take about 5 to 11 ms over 50,000 turns. A semantic fallback scans indexed turns linearly, so a miss gets slower as the index grows.

The [FAQ](docs/faq.md) has the data paths, privacy limits, token details, semantic search details, proof commands, and performance data.

<p align="center">
  <img src="assets/demo.gif" alt="subrosa demo: search the archive, automatic recall on a prompt, dashboard" width="800">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#what-it-does">What it does</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#the-memory-workflow">Memory workflow</a> ·
  <a href="#make-claude-use-the-archive-itself">Claude instructions</a><br/>
  <a href="docs/comparison.md">Comparison</a> ·
  <a href="docs/faq.md">FAQ</a>
</p>

## Why "subrosa"?

The name comes from the Latin phrase `sub rosa`, meaning private or confidential.

## Quick start

Inside Claude Code, run:

```
/plugin marketplace add ij5a/subrosa
/plugin install subrosa@subrosa
```

Start a new Claude Code session after installation. The plugin downloads the right prebuilt program for your computer, about 2.5 MB, and checks its checksum.

It also archives sessions already on your disk. Later, it archives sessions while you work and when they end. It shows related past sessions in Claude's context and reports sessions waiting for long-term memory.

The program download fetches only the program. Your data stays on your machine.

Run this command to add the optional Claude instructions:

```sh
~/.claude/subrosa/bin/subrosa init --claude-md   # or: subrosa init --claude-md
```

The instructions make Claude search the archive at task start and process queued checkpoints. The command is safe to run again and adds only missing sections. It adds about 250 tokens of context.

### Optional: install the `subrosa` command

The plugin works without the CLI. Install the CLI to search and manage the archive yourself.

```sh
brew install ij5a/tap/subrosa
curl -fsSL https://raw.githubusercontent.com/ij5a/subrosa/main/install.sh | sh
cargo install --git https://github.com/ij5a/subrosa
```

The plugin uses a `subrosa` binary on PATH when one exists. Use these commands first:

```sh
subrosa setup    # choose a backup mirror, or none
subrosa          # open the dashboard
```

## What it does

- Archives sessions when Claude Code ends them with quit, `/clear`, or logout. A start-up sweep catches changed transcripts.
- Archives the live session after each assistant turn. The current session becomes searchable before it ends.
- Adds up to 3 strong keyword matches from the same project to a prompt. It skips weak matches and the current session.
- Searches with FTS5. Use `--project`, `--after`, `--before`, `--tag`, `--context`, `--exclude`, `--any`, and `--fuzzy` to narrow results.
- Supports semantic search with `subrosa search --semantic`. Automatic semantic search runs only after a plain search has zero hits and the local model and index are ready.
- Builds curated facts with `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog`. Each fact has a small Markdown file. `subrosa generate` writes the size-limited `MEMORY.md`, and `subrosa fact search` finds facts.
- Lists sessions and deterministic tags. Tags include tools, file types, and topic terms.
- Finds co-occurring terms with `subrosa related`. It supports linked facts and read-only checks with `subrosa fact link` and `subrosa fact doctor`.
- Makes throttled snapshots. An optional mirror can encrypt the latest snapshot with XChaCha20-Poly1305 and argon2id.
- Masks private-key blocks, AWS keys, bearer tokens, and labeled secret values before storage.

## Commands

```sh
subrosa                                  # dashboard, same as: subrosa stats
subrosa search aurora failover           # keyword search
subrosa search --project api deploy      # project scope
subrosa search -n 30 --raw 'cache OR redis' # raw FTS5 query
subrosa search --fuzzy ratelimiter       # partial names and small typos
subrosa search deploy --after 2026-05-01 # YYYY-MM-DD, inclusive; use --before too
subrosa search api --tag tool:kubectl    # tag filter; repeat --tag to require all
subrosa search pgbouncer -C 2            # show nearby turns
subrosa search timeout --exclude test    # exclude a term; repeat to add more
subrosa search redis valkey --any        # match any term, not all terms
subrosa embed                            # update the semantic index
subrosa embed --rebuild                  # rebuild semantic vectors
subrosa search --semantic 'why did checkout get slow' # semantic search
subrosa related cache-prod               # co-occurring terms and sessions
subrosa related TICKET-123 --project api # scoped related search
subrosa sessions                         # sessions, newest first
subrosa sessions --tag topic:cache-prod --after 2026-05-01
subrosa session <id> --tags              # session and derived tags
subrosa fact list                        # facts for the current project
subrosa fact search pgbouncer            # search facts
subrosa fact upsert --leaf note.md       # add or update one fact
subrosa fact link auth-decision          # linked facts and dead links
subrosa fact doctor                      # read-only; exit 1 on a break
subrosa generate                         # rebuild MEMORY.md
subrosa import ~/.claude/projects/<project>/memory # import an existing MEMORY.md
subrosa session <id>                     # full ID or unique prefix
subrosa pending                          # queued checkpoints
subrosa checkpoint-drop <id>             # remove one queued session
subrosa sweep                            # catch up on transcripts
subrosa backup --force                   # make a snapshot now
subrosa restore <mirror>/subrosa-latest.db.enc   # decrypt an encrypted snapshot
subrosa setup                            # choose the backup mirror
```

## The memory workflow

1. When a session ends, subrosa archives it and adds it to the checkpoint queue.
2. At the next start, Claude receives an `ACTION REQUIRED` note for queued sessions. The note goes to Claude's context, not your chat window.
3. The note repeats on each prompt until the queue clears. Set `checkpoint_nudge=quiet` or `off` to change the reminder.
4. Run `/subrosa:checkpoint-backlog` to save durable facts from queued sessions. Run `/subrosa:checkpoint` before `/clear` or `/compact` to save the live session.
5. `subrosa generate` builds `MEMORY.md` under a byte budget. Pinned facts and feedback win when space is limited. Other facts stay searchable in the archive.

## Make Claude use the archive itself

Recall runs only when you type a prompt. Add these sections to `CLAUDE.md` so Claude also searches at task start and handles queued checkpoints.

`~/.claude/CLAUDE.md` covers every project. A repository `CLAUDE.md` covers that repository. Run `subrosa init --claude-md` to add both sections safely.

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

A plain exact miss also tries semantic search when automatic indexing is on and its local index is available. Use `--fuzzy` for partial names and small typos.

```markdown
## Memory auto-checkpoint (subrosa)

When a `[subrosa] ACTION REQUIRED` note says sessions are queued for checkpoint
(or `subrosa pending` is non-empty), run the `/subrosa:checkpoint-backlog` skill
in the background — never before or blocking the task you're working on. It saves
the durable facts from each queued session into that project's memory, then clears
the queue as it finishes. Skip it silently when nothing is queued.
```

The first section makes Claude search during a task. The second clears queued checkpoints in the background.

## Where your data lives

See [Where is my data?](docs/faq.md#where-is-my-data) for paths, permissions, config, and mirror rules.

## Privacy model

See [Can my data leave my machine?](docs/faq.md#can-my-data-leave-my-machine) and [What does subrosa not protect?](docs/faq.md#what-does-subrosa-not-protect).

## Proof: verify it yourself

See [Proof](docs/faq.md#proof) for commands that check token limits, network behavior, model pinning, and dependencies.

## Performance

See [Performance](docs/faq.md#performance) for measured times and the archive-size limits.

## Development

```sh
mise install
git config core.hooksPath .githooks
mise exec -- cargo fmt --check
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo test --locked
```

The golden tests pin stored text, session dumps, `MEMORY.md`, recall output, and other output formats byte for byte. Use a throwaway directory for manual tests:

```sh
mise exec -- env SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
```

## License

MIT
