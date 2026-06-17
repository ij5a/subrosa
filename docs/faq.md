# FAQ

## Can my data leave my machine?

Your data — no. The binary makes zero network calls: no cloud, no telemetry, no update checker. Two network things happen, neither sends your data out: the plugin's one-time bootstrap downloads the program from GitHub releases (sha256-verified against checksums in this repo), and the optional backup mirror copies a snapshot into a folder you pick. Aim that mirror at iCloud or Dropbox and your sync client uploads the snapshot — your choice, off by default, and only static snapshots ever land there, never the live database.

## Where is my data?

`~/.claude/subrosa/memory.db` (live database), `~/.claude/subrosa/backups/` (last 7 snapshots), plus the mirror folder if you set one. The database and its folder are readable only by you (`0600`/`0700`). All paths are env-overridable (`SUBROSA_DIR`, `SUBROSA_DB`, …).

## What gets redacted?

Private key blocks, AWS access keys, bearer tokens, and `password=` / `token:`-style values are masked before storage — only the secret part is hidden, the rest stays searchable. The original transcripts under `~/.claude/projects` stay as Claude Code wrote them; full-disk encryption is the at-rest control for those.

## Is the archive encrypted at rest?

No — by design; full-disk encryption is the control. The database is `0600` in a `0700` directory, so FileVault (macOS) or LUKS (Linux) encrypts it at rest along with everything else. subrosa adds no encryption of its own: it runs unattended, so the key would have to sit next to the data, readable by the same user — full-disk encryption with extra steps and no real gain. The transcripts Claude Code writes under `~/.claude/projects` are [plaintext regardless](https://code.claude.com/docs/en/claude-directory#plaintext-storage).

## What does subrosa not protect?

Honest limits, so you know what you're getting:

- **Redaction matches known shapes, not everything.** Private-key blocks, AWS keys, `Bearer` tokens, and labeled secrets (`password=`, `token:`) are masked; a `ghp_…` token, an `sk-…` key, or a bare JWT is stored as written. It cuts obvious leaks, not all of them.
- **The archive isn't encrypted** — `0600`/`0700` is access control, enforced on Unix only (Windows falls back to default ACLs). Full-disk encryption is the real at-rest control.
- **Recall re-injects your own stored text.** On a strong match it puts up to 3 snippets back into context, so anything that did leak into the archive can resurface — the injection block tells Claude to treat it as unverified.
- **Original transcripts stay cleartext** under `~/.claude/projects`; subrosa never edits those.
- **One snapshot can leave the machine, by your choice** — the opt-in backup mirror, if you point it at a synced folder. The live database is never synced.

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
| `MEMORY.md` index load | once per session start | **≤ 23 KB (≈ 6k tokens)**, usually far less |
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

Yes. After each assistant turn subrosa archives the in-progress transcript (the `Stop` hook), so the live session shows up in `subrosa search` before you ever end it — handy when a second terminal or agent needs to see what this one is doing, or when you switch sessions fast and want the one you just left already archived. It costs ~7 ms per turn and runs after the reply is on screen, so you never feel it. The automatic prompt recall still skips the current session on purpose — it's already in Claude's context, so re-injecting it would only burn tokens — so this helps *across* sessions, not echo the one you're in.

## Why keyword search instead of embeddings?

A deliberate trade. Embeddings (meaning-based search) need model weights or API calls — that breaks the single static binary, the 5-crate supply chain, and zero-cost capture. Instead subrosa matches word roots, so `deploy` finds `deployed` and `deploying`, while identifiers like `TICKET-123` and `my-app-prod` stay exact. For partial names and typos, `subrosa search --fuzzy` adds a local trigram index (built on first use) — still no model. Meaning-based search stays the deliberate omission.

## What are session tags?

Short labels subrosa derives for each session at archive time — locally, no LLM, zero tokens. Three kinds: `tool:bash` (tools the session used), `ext:rs` (file types it touched), and `topic:cache-prod` (the distinctive terms it was about). They let you filter the archive without remembering a keyword: `subrosa sessions --tag tool:kubectl`, or `subrosa search deploy --tag topic:aurora`. They're read-only by design — recomputed from the stored (already-redacted) text every time a session is archived, never hand-edited. See one session's tags with `subrosa session <id> --tags`.

## How do I verify the binary?

Releases are built by GitHub Actions from a tagged commit and published with `sha256sums.txt`. The plugin bootstrap verifies its download against checksums committed here (`hooks/sha256sums.txt`), and the Homebrew formula pins the same hashes. Or skip all of it — `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Remove the plugin (`/plugin uninstall subrosa@subrosa`) and, if you want the data gone, delete `~/.claude/subrosa/` and your mirror folder. Nothing else is touched — no services, no launch agents, no shell-profile edits.

## What do you collect about me?

Nothing. There's no telemetry to opt out of. The only analytics are what GitHub shows everyone: stars, traffic, release download counts.
