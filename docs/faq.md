# FAQ

## Can my data leave my machine?

Your data — no. The binary makes zero network calls: no cloud, no telemetry, no update checker. Two network things happen, but neither sends your data out: the plugin's one-time bootstrap downloads the program from GitHub releases (sha256-verified against checksums committed in this repo), and the optional backup mirror copies a snapshot file into a folder you pick. If you point the mirror at an iCloud or Dropbox folder, your sync client uploads that snapshot — that's your choice, off by default, and only static snapshot files ever land there (never the live database).

## Where is my data?

`~/.claude/subrosa/memory.db` (live database), `~/.claude/subrosa/backups/` (snapshots, last 7), plus the mirror folder if you configured one. The database is readable only by you (`0600`), and so is its folder (`0700`). All paths are env-overridable (`SUBROSA_DIR`, `SUBROSA_DB`, …).

## What gets redacted?

Private key blocks, AWS access keys, bearer tokens, and `password=` / `token:`-style values are masked before storage — only the secret part is hidden; the rest stays searchable. The original transcripts under `~/.claude/projects` remain as Claude Code wrote them; full-disk encryption is the at-rest control for those.

## Is the archive encrypted at rest?

No — by design, and full-disk encryption is the control. The database is `0600` in a `0700` directory; turn on FileVault (macOS) or LUKS (Linux) and it's encrypted at rest along with everything else, including the original transcripts. subrosa doesn't add its own encryption because the program runs on its own, with no one there to type a password — so the key would have to sit right next to the data, readable by the same user. That's full-disk encryption with extra steps, no real gain against an attacker who's already that user. And the transcripts Claude Code writes under `~/.claude/projects` are [plaintext regardless](https://code.claude.com/docs/en/claude-directory#plaintext-storage), so encrypting only subrosa's copy would protect little. The one thing that can leave the machine is a mirror snapshot you opt into syncing — keep that folder off, or point it at one that's itself encrypted.

## How many tokens does it cost me?

Almost nothing — and the cost is fixed by constants in the code, not by how much you use it.

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
- 2026-05-20 · 7f3a9c2: rotating the «aurora» reader creds broke «pgbouncer» auth — bounce the pooler after the secret swap, not before
- 2026-04-11 · c81e0d4: «aurora» failover test: replica promotion took ~40s, within SLO
```

Claude reads those snippet lines as leads — it doesn't open the full transcripts. If a lead is worth more, Claude (or you) pulls that one session with `subrosa session <id>` — a separate, on-demand step, not automatic.

Outside the per-prompt loop: `MEMORY.md` loads once at session start (**≤ 23 KB** of always-loaded context, like `CLAUDE.md` — subrosa builds it for `0`, Claude reads it once); archiving the session at the end is `0`; and `/subrosa:checkpoint` is Claude's own work (**N**), though you trigger it and the `subrosa` CLI calls it makes are `0`.

**Bottom line:** subrosa never spends your model tokens on its own work — tokens flow only when Claude reads what it surfaced (capped: ~180 per prompt, plus the ≤23 KB once-per-session index) or does your actual task.

Every operation and what it costs:

| Operation | When it runs | Token cost |
|---|---|---|
| Saving a session | session end + catch-up at start | **0** — mechanical parsing, no LLM |
| Auto-recall, no match | every prompt (the usual case) | **0** — stays silent |
| Auto-recall, strong match | every prompt | **~180** — top 3 snippets, hard-capped |
| `MEMORY.md` index load | once per session start | **≤ 23 KB (≈ 6k tokens)**, usually far less |
| `subrosa search` | only when you or Claude run it | **~a few hundred** (15 ranked lines), net-cheaper than rediscovery |

**Example — a 30-prompt session.** The index loads once at the start (a few KB for a typical project — this is normal always-loaded context, like `CLAUDE.md`). Across the 30 prompts, most don't match anything in your past work, so nothing is injected; recall only fires on a strong match (at least 2 matching words, one of them a specific name or ID), and each fire adds ~180 tokens. Say 4 of them match: ~720 tokens for the whole session. Saving all 30 turns when you quit: 0. Ask Claude to dig into the archive twice → two `subrosa search` calls, a few hundred tokens each, each replacing a multi-thousand-token rediscovery.

So the cost is flat and predictable — a small one-time index load plus capped, usually-silent recall. There's no per-save LLM bill that grows with how much you do. For how this token cost compares to LLM-summarizing plugins, see the [comparison](comparison.md).

## Does it get more expensive or slower as the archive grows?

No — both stay flat. Token cost doesn't scale with size: saving is always zero tokens, and recall is capped at ~180 per prompt whether you have 100 sessions or 100,000 — the cap is a constant, not a percentage. Search is an FTS5 index, not a linear scan, so it stays fast as data grows — ~5–11 ms over a 50,000-turn archive. Disk grows only by text: a year of heavy use is a few hundred MB. When the auto-recall snippets aren't enough, Claude runs `subrosa search` on demand — a bounded, ranked result list (default 15 lines), which costs far fewer tokens than rediscovering the answer from scratch. Run [`scripts/bench.sh`](../scripts/bench.sh) to confirm the latency and recall-token numbers on your own machine.

## Why don't I see subrosa's messages in the chat?

That output isn't a chat message. Claude Code puts it straight into *Claude's context* (at session start, and when you submit a prompt) — Claude sees and acts on it, but it never shows up as a message to you. The dashboard (`subrosa`) and `subrosa search` are the views meant for you to read.

## Recall only shows 3 results — doesn't it miss things?

The automatic recall is a small, cheap nudge, not the full answer. It injects the top 3 matches (~180 tokens) ranked by relevance, capped so it can't quietly grow your per-prompt cost. Treat them as leads, not the last word — they're ranked by keyword match (with fresher sessions breaking near-ties), not by meaning, and the snippet is centered on the matching text so you can see why it surfaced. The injected note tells Claude to check them before relying on them. When 3 aren't enough, Claude (or you) runs `subrosa search` for the full ranked list — widen it with `-n 20` — and nothing is ever lost: the whole archive stays searchable no matter what recall surfaced.

## Why keyword search instead of embeddings?

A deliberate trade. Embeddings (meaning-based search) need model weights or API calls — that breaks the single static binary, the 5-crate supply chain, and zero-cost capture. Instead it matches word roots, so `deploy` finds `deployed` and `deploying`, while identifiers like `TICKET-123` and `my-app-prod` stay exact. For partial names and typos, `subrosa search --fuzzy` adds a local trigram index (built on first use) — still no model. True meaning-based (semantic) search stays the deliberate omission.

## How do I verify the binary?

Releases are built by GitHub Actions from a tagged commit and published with `sha256sums.txt`. The plugin bootstrap verifies its download against checksums committed in this repo (`hooks/sha256sums.txt`), and the Homebrew formula pins the same hashes. Or skip all of it: `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Remove the plugin (`/plugin uninstall subrosa@subrosa`) and, if you want the data gone, delete `~/.claude/subrosa/` and your mirror folder. Nothing else is touched — no services, no launch agents, no shell profile edits.

## What do you collect about me?

Nothing. There's no telemetry to opt out of. Our only analytics are what GitHub shows everyone: stars, traffic, release download counts.
