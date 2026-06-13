# FAQ

## Does anything ever leave my machine?

Your data — no. The binary makes zero network calls: no cloud, no telemetry, no update checker. Two network touches exist around it, and neither carries your data out: the plugin's one-time bootstrap downloads the program from GitHub releases (sha256-verified against checksums committed in this repo), and the optional backup mirror copies a snapshot file into a folder you pick. If you point the mirror at an iCloud or Dropbox folder, your sync client uploads that snapshot — that's your choice, off by default, and only static snapshot files ever land there (never the live database).

## Where is my data?

`~/.claude/subrosa/memory.db` (live database), `~/.claude/subrosa/backups/` (snapshots, last 7), plus the mirror folder if you configured one. The database is `0600` in a `0700` directory. All paths are env-overridable (`SUBROSA_DIR`, `SUBROSA_DB`, …).

## What gets redacted?

Private key blocks, AWS access keys, bearer tokens, and `password=` / `token:`-style values are masked before storage — only the secret value is replaced, surrounding words stay searchable. The original transcripts under `~/.claude/projects` remain as Claude Code wrote them; full-disk encryption is the at-rest control for those.

## Is the archive encrypted at rest?

No — by design, and full-disk encryption is the control. The database is `0600` in a `0700` directory; turn on FileVault (macOS) or LUKS (Linux) and it's encrypted at rest along with everything else, including the original transcripts. subrosa doesn't add its own encryption because an automated hook has to open the database with no one there to type a passphrase — so the key would sit next to the data, under the same user and permissions. That's full-disk encryption with extra steps, no real gain against an attacker who's already that user. And the transcripts Claude Code writes under `~/.claude/projects` are [plaintext regardless](https://code.claude.com/docs/en/claude-directory), so encrypting only subrosa's copy would protect little. The one thing that can leave the machine is a mirror snapshot you opt into syncing — keep that folder off, or point it at one that's itself encrypted.

## How many tokens does it cost me?

Recall injects at most ~180 tokens per prompt, and usually injects nothing — it stays silent unless your prompt shares at least two distinctive terms (one identifier-grade) with a past session, and it never re-injects the same session into one conversation. The always-loaded `MEMORY.md` index is hard-capped at 23 KB. Saving sessions costs zero tokens: archiving is mechanical parsing, no LLM involved. Run [`scripts/bench.sh`](../scripts/bench.sh) to verify the latency numbers on your machine.

## Does it get more expensive or slower as the archive grows?

No — both stay flat. Token cost doesn't scale with size: saving is always zero tokens, and recall is capped at ~180 per prompt whether you have 100 sessions or 100,000 — the cap is a constant, not a percentage. Search is an FTS5 index, not a linear scan, so it stays fast as data grows — ~5–11 ms over a 50,000-turn archive. Disk grows only by text: a year of heavy use is a few hundred MB. When the auto-recall snippets aren't enough, Claude runs `subrosa search` on demand — a bounded, ranked result list (default 15 lines), which costs far fewer tokens than rediscovering the answer from scratch.

## Why don't I see subrosa's messages in the chat?

Hook output isn't chat. Claude Code adds SessionStart and UserPromptSubmit hook stdout to *Claude's context* — Claude sees and acts on it, but it doesn't render as a message to you. The dashboard (`subrosa`) and `subrosa search` are the human-facing views.

## Why keyword search instead of embeddings?

A deliberate trade. Embeddings need model weights or API calls — that breaks the single static binary, the 5-crate supply chain, and zero-cost capture. Porter stemming covers word forms (`deploy` finds `deployed`/`deploying`) while identifiers like `TICKET-123` and `my-app-prod` stay exact-match. If real-world misses pile up, an opt-in trigram index is the planned fallback — still local, still no model.

## How do I verify the binary?

Releases are built by GitHub Actions from a tagged commit and published with `sha256sums.txt`. The plugin bootstrap verifies its download against checksums committed in this repo (`hooks/sha256sums.txt`), and the Homebrew formula pins the same hashes. Or skip all of it: `cargo install --git https://github.com/ij5a/subrosa` builds from source.

## What does uninstalling leave behind?

Remove the plugin (`/plugin uninstall subrosa@subrosa`) and, if you want the data gone, delete `~/.claude/subrosa/` and your mirror folder. Nothing else is touched — no services, no launch agents, no shell profile edits.

## What do you collect about me?

Nothing. There's no telemetry to opt out of. Our only analytics are what GitHub shows everyone: stars, traffic, release download counts.
