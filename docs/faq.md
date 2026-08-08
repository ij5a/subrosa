# FAQ

## Can my data leave my machine?

Not to us or to anyone else: no cloud, no telemetry, no update checker, and the binary never opens a socket itself. Three network things exist, and you switch on all three. The plugin's one-time bootstrap downloads the program from GitHub releases (sha256-verified against checksums in this repo). The optional backup mirror copies a snapshot into a folder you pick — aim it at iCloud or Dropbox and your sync client uploads the snapshot, your choice, off by default, only static snapshots ever land there, never the live database, and it goes out encrypted if you set a mirror passphrase. And the opt-in `subrosa embed` downloads its model from Hugging Face the first time you run it, by handing three URLs to your own `curl` — a download only, nothing of yours goes up. [Why keyword search](#why-keyword-search-instead-of-embeddings) has the details.

## Where is my data?

`~/.claude/subrosa/memory.db` (live database), `~/.claude/subrosa/backups/` (last 7 snapshots), plus the mirror folder if you set one. The database and its folder are readable only by you (`0600`/`0700`). All paths are env-overridable (`SUBROSA_DIR`, `SUBROSA_DB`, …).

## What gets redacted?

Private key blocks, AWS access keys, bearer tokens, and `password=` / `token:`-style values are masked before storage — only the secret part is hidden, the rest stays searchable. A `passphrase=` line is the exception: a passphrase is allowed to contain spaces, so there's no way to tell where it ends, and everything to the end of that line is masked. That covers subrosa's own mirror passphrase however you typed it — quoted, unquoted, in an `export`, or in the config file. The original transcripts under `~/.claude/projects` stay as Claude Code wrote them; full-disk encryption is the at-rest control for those.

## Is the archive encrypted at rest?

The live database, no — by design; full-disk encryption is the control. The database is `0600` in a `0700` directory, so FileVault (macOS) or LUKS (Linux) encrypts it at rest along with everything else. subrosa adds no encryption of its own there: it runs unattended, so the key would have to sit next to the data, readable by the same user — full-disk encryption with extra steps and no real gain. The transcripts Claude Code writes under `~/.claude/projects` are [plaintext regardless](https://code.claude.com/docs/en/claude-directory#plaintext-storage).

The mirror copy is different, because that one leaves your machine. Set `mirror_passphrase` (in `subrosa setup`, the config file, or `SUBROSA_MIRROR_PASSPHRASE`) and the mirrored snapshot is encrypted with XChaCha20-Poly1305, key from argon2id — it lands as `subrosa-latest.db.enc` and `subrosa restore <file>` reads it back. Eight things to know:

- It protects the cloud copy only. Everything on your own disk — live database, local snapshots, original transcripts — is untouched, so full-disk encryption is still the control there.
- The passphrase sits in `~/.claude/subrosa/config`, mode `0600`, so it's readable by anyone who is already you on this machine. That's fine for its job: keeping the cloud provider (and anyone who gets at your synced folder) out.
- Turning it on doesn't erase what you already synced. Your cloud folder's trash and version history can still hold the old plaintext `subrosa-latest.db` — purge it there yourself. subrosa only deletes the plaintext copy sitting in the folder.
- Every encrypted snapshot is a full upload. Each one gets a fresh salt and nonce, so no two are alike and your sync client can't send only the changed blocks — the whole file goes up each time.
- Turning it off has to be deliberate. Once a `subrosa-latest.db.enc` exists, dropping the passphrase does not go back to plaintext: the backup reports the missing passphrase and skips the mirror, so a lost config line can never quietly publish your archive in the clear. To go back, delete `subrosa-latest.db.enc` from the mirror folder yourself — and if iCloud has evicted it, the file to delete is the hidden `.subrosa-latest.db.enc.icloud` placeholder standing in for it.
- Sync-conflict copies are yours to clean up. If your cloud client ever made a second file — names like `subrosa-latest 2.db` or `subrosa-latest (conflicted copy).db` — subrosa does not delete it. It only removes the exact `subrosa-latest.db` it wrote, because widening that to a name pattern would put your own files in range. Check the mirror folder once after turning encryption on.
- Setting it outside `subrosa setup` takes effect at the next backup. Add the passphrase by hand (env var or config file) and nothing changes right away: the plaintext copy goes at the next session end, and the first `.enc` appears when the 24-hour throttle allows. Run `subrosa backup --force` to seal it immediately.
- `none` switches mirroring off whichever side says it. `mirror=none` in the config overrides a `SUBROSA_MIRROR` variable — that line is only ever written by a deliberate opt-out, so a shell profile can't quietly undo it — and `SUBROSA_MIRROR=none` overrides a configured folder the same way. Delete the line, unset the variable, or re-run `subrosa setup`, to mirror again. Either way it only stops subrosa writing there: if the other side still names a real folder, `subrosa restore` goes on refusing to put a readable copy into it, because opting out of a backup doesn't make that folder any less cloud-synced.

## What does subrosa not protect?

Honest limits, so you know what you're getting:

- **Redaction matches known shapes, not everything.** Private-key blocks, AWS keys, `Bearer` tokens, and labeled secrets (`password=`, `token:`) are masked; a `ghp_…` token, an `sk-…` key, or a bare JWT is stored as written. It cuts obvious leaks, not all of them.
- **The archive on disk isn't encrypted** — `0600`/`0700` is access control, enforced on Unix only (Windows falls back to default ACLs). Full-disk encryption is the real at-rest control. Only the mirror copy can be encrypted, and only if you set a passphrase.
- **Recall re-injects your own stored text.** On a strong match it puts up to 3 snippets back into context, so anything that did leak into the archive can resurface — the injection block tells Claude to treat it as unverified.
- **Original transcripts stay cleartext** under `~/.claude/projects`; subrosa never edits those.
- **One snapshot can leave the machine, by your choice** — the opt-in backup mirror, if you point it at a synced folder. Set a mirror passphrase and it goes out encrypted; without one it's a readable copy of your archive in someone else's cloud. The live database is never synced.

## How many tokens does it cost me?

Almost nothing — the cost is fixed by constants in the code, not by how much you use it.

**Step by step, for one prompt:**

1. **You type a prompt** — `0` extra; subrosa adds nothing to your message.
2. **subrosa recall runs** (local search + a relevance gate) — `0`, no model call.
3. **subrosa adds any strong match to context** — `0` on a weak match (the usual case), otherwise **~180 tokens**.
   **← Claude takes over here.** Steps 1–3 are subrosa: local, mechanical, zero model tokens.
4. **Claude reads the injected notes + your prompt** — the ~180 tokens from step 3 (`0` if nothing matched).
5. **Claude does the task** (reads files, reasons, writes) — **N tokens**: normal usage, nothing to do with subrosa.

An injected block (step 3) looks like this — just the snippet lines, not the full sessions:

```
[subrosa recall] Possibly relevant past sessions from the local archive — verify before relying on them; run `subrosa search` for the full text:
- 2026-05-20 (3w old) · 7f3a9c2: rotating the «aurora» reader creds broke «pgbouncer» auth — bounce the pooler after the secret swap, not before
- 2026-04-11 (2mo old) · c81e0d4: «aurora» failover test: replica promotion took ~40s, within SLO
```

Each line shows the session date and how old it is, so Claude leans on fresh hits and double-checks stale ones, and reads them as leads — not full transcripts. Worth more? Pull that one session with `subrosa session <id>`, on demand.

Every operation and what it costs:

| Operation | When it runs | Token cost |
|---|---|---|
| Saving a session | session end + catch-up at start | **0** — mechanical parsing, no LLM |
| Auto-recall, no match | every prompt (the usual case) | **0** — stays silent |
| Auto-recall, strong match | every prompt | **~180** — top 3 snippets, hard-capped |
| `MEMORY.md` index load | once per session start | **≤ 23 KB (≈ 6k tokens)** by default, usually far less — raise per project with `echo <n> > <memdir>/.budget` |
| `subrosa search` | only when you or Claude run it | **~a few hundred** (15 ranked lines), net-cheaper than rediscovery |
| `subrosa sessions` | only when you or Claude run it | **~a few hundred** (a page of session lines), like search |
| Deriving session tags | session end + catch-up at start | **0** — local text parsing, no LLM |

**Example — a 30-prompt session.** The index loads once at the start; most prompts match nothing, so nothing is injected; recall fires only on a strong match (~180 tokens). Say 4 match: ~720 tokens for the session, plus `0` to save all 30 turns on quit. Flat and predictable — no per-save LLM bill that grows with use. For how this compares to LLM-summarizing plugins, see the [comparison](comparison.md).

## Does it get more expensive or slower as the archive grows?

No — both stay flat. Token cost doesn't scale with size: saving is always zero, and recall is capped at ~180 per prompt whether you have 100 sessions or 100,000 — the cap is a constant, not a percentage. Search is an FTS5 index, not a linear scan, so it stays fast — ~5–11 ms over a 50,000-turn archive. Disk grows only by text: a year of heavy use is a few hundred MB. Run [`scripts/bench.sh`](../scripts/bench.sh) to confirm the latency and recall-token numbers yourself.

## Why don't I see subrosa's messages in the chat?

That output isn't a chat message — Claude Code puts it straight into *Claude's context* (at session start, and when you submit a prompt). Claude sees and acts on it, but it never shows up as a message to you. The dashboard (`subrosa`) and `subrosa search` are the views meant for you to read.

## Recall only shows 3 results — doesn't it miss things?

The automatic recall is a small, cheap nudge, not the full answer — the top 3 matches (~180 tokens) ranked by relevance, capped so it can't quietly grow your per-prompt cost. Treat them as leads: they're ranked by keyword match (fresher sessions break near-ties), not meaning, and each snippet is centered on the matching text so you see why it surfaced. When 3 aren't enough, Claude (or you) runs `subrosa search` for the full ranked list (`-n 20` to widen it) — nothing is ever lost, the whole archive stays searchable.

## Is my current session searchable while it's still running?

Yes. After each assistant turn subrosa archives the in-progress transcript (the `Stop` hook), so the live session shows up in `subrosa search` before you ever end it — handy when a second terminal or agent needs to see what this one is doing, or when you switch sessions fast and want the one you just left already archived. It costs ~7 ms per turn — flat no matter how long the session runs, since it resumes from a saved offset and reads only the new lines — and runs after the reply is on screen, so you never feel it. The automatic prompt recall still skips the current session on purpose — it's already in Claude's context, so re-injecting it would only burn tokens — so this helps *across* sessions, not echo the one you're in.

## Why keyword search instead of embeddings?

Keyword is the default because it needs nothing — no model, no weights, no second process, no cost to save a session. subrosa matches word roots, so `deploy` finds `deployed` and `deploying`, while identifiers like `TICKET-123` and `my-app-prod` stay exact. For partial names and typos, `subrosa search --fuzzy` adds a local trigram index (built on first use); when no substring matches, it falls back to the nearest matches within one edit (a wrong, missing, extra, or swapped letter) — still no model.

Meaning-based search is there when you want it, as something you turn on yourself. There's nothing to install — the model runs inside the same binary:

1. Run `subrosa embed` once. The first run downloads the model it uses ([`BAAI/bge-large-en-v1.5`](https://huggingface.co/BAAI/bge-large-en-v1.5), MIT licensed, ~1.3 GB) into `~/.claude/subrosa/models/`, through your own `curl`, checked against a sha256 pinned in the source. Then it embeds each archived turn and stores the vector beside it — this is CPU work, so a big archive takes a while, and it's resumable: Ctrl-C just means the next run finishes the rest. Re-run it after new sessions pile up, since only embedded turns can be ranked — `--semantic` warns you whenever some were left out and tells you how many. `subrosa embed --rebuild` throws away what's stored and starts over, which is the fix if a search ever reports unreadable vectors.
2. `subrosa search --semantic "why did checkout get slow"` ranks by meaning: a turn can surface without sharing a single word with your query.

Nothing you typed leaves the machine. The download only pulls files down; your turns and your queries are embedded locally, and the query is redacted before the model sees it, same as the archived text was. If the model isn't on disk and can't be fetched, `--semantic` says so, prints the three URLs to grab by hand, and stops — it never quietly falls back to keyword. Save a hand-download under the `.part` name it shows you: subrosa checksums those and renames them into place itself, and a file dropped straight at its final name is only checked by size.

The model is English-only. Non-English text still embeds and still ranks, just less well; keyword search treats every language the same, so that stays the better tool there.

Automatic recall stays keyword, always. It fires on every prompt and has to be silent and instant when nothing matches — putting a model round-trip in front of every message you type is the opposite of that. Semantic is something you ask for, never something that happens to you.

## What are session tags?

Short labels subrosa derives for each session at archive time — locally, no LLM, zero tokens. Three kinds: `tool:bash` (tools the session used), `ext:rs` (file types it touched), and `topic:cache-prod` (the distinctive terms it was about). They let you filter the archive without remembering a keyword: `subrosa sessions --tag tool:kubectl`, or `subrosa search deploy --tag topic:aurora`. They're read-only by design — recomputed from the stored (already-redacted) text every time a session is archived, never hand-edited. See one session's tags with `subrosa session <id> --tags`.

## How do I verify the binary?

Releases are built by GitHub Actions from a tagged commit and published with `sha256sums.txt`. The plugin bootstrap verifies its download against checksums committed here (`hooks/sha256sums.txt`), and the Homebrew formula pins the same hashes. Or skip all of it — `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Remove the plugin (`/plugin uninstall subrosa@subrosa`) and, if you want the data gone, delete `~/.claude/subrosa/` and your mirror folder. Nothing else is touched — no services, no launch agents, no shell-profile edits.

## What do you collect about me?

Nothing. There's no telemetry to opt out of. The only analytics are what GitHub shows everyone: stars, traffic, release download counts.
