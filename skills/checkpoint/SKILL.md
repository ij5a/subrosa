---
name: checkpoint
description: Flush durable facts from the current conversation into subrosa's memory before /clear or /compact. Scans the session for user, feedback, project, and reference items; writes or updates leaf memory files and the facts database, then regenerates the byte-budgeted MEMORY.md; soft-archives stale entries; reports a short session recap and confirms it's safe to wipe.
---

# checkpoint — flush memory before a context wipe

The user is about to run `/clear` or `/compact`. Without saving first, `/clear` loses everything from this session and `/compact`'s lossy summary will drop the load-bearing details. Your job is to scan the conversation, save anything durable, and report back so the user can confirm it's safe to wipe.

`MEMORY.md` is **generated** from the facts database (`~/.claude/subrosa/memory.db`) — never hand-edit it. You write leaf files and register facts; the generator rebuilds the byte-budgeted index. All commands below use the `subrosa` CLI (`subrosa fact …`, `subrosa generate`, `subrosa search …`). If `subrosa` isn't on PATH, the plugin's bootstrap installs it at `~/.claude/subrosa/bin/subrosa`.

## Procedure

1. **Scan the full conversation** (not just the last turn) for memory-worthy items. Match each candidate to one of the four memory types:
   - **user** — role, preferences, knowledge, working context
   - **feedback** — corrections (don't do X) and validated approaches (yes, that was right). Both matter; corrections are easy to spot, confirmations are quieter.
   - **project** — ongoing work, deadlines, motivations, who's doing what, why
   - **reference** — pointers to external systems, dashboards, ticket projects

2. **Apply the exclusion list strictly.** Do not save:
   - Code patterns, conventions, file paths, architecture
   - Git history, commit hashes, blame info, PR numbers
   - Debugging recipes (the fix is in the code; the why is in the commit message)
   - Anything already covered by `CLAUDE.md`
   - Ephemeral state (in-progress task details, current investigation chain, ticket numbers without surrounding context)
   - Routine activity logs — only save what was *surprising* or *non-obvious*

   These exclusions hold even if the user said "save this." If the user asked to save excluded content, push back briefly: ask what was non-obvious about it, or whether they meant to add it to `CLAUDE.md` instead.

3. **Check existing memory before writing.** Query the facts DB for anything related — `subrosa fact list` to scan the curated facts, `subrosa search "<keyword>"` to search the full transcript archive — and read the related leaf files. For each candidate:
   - Similar fact exists and is correct → skip (don't duplicate)
   - Similar fact exists but is stale → update it in place
   - No similar fact exists → write a new one

4. **Write the leaf, then register the fact (do NOT hand-edit `MEMORY.md` — it is generated):**
   - Create or update the leaf file with frontmatter: `name`, `description`, `type`
   - Use the **Why:** / **How to apply:** structure for `feedback` and `project` types — the *why* is what lets future-you judge edge cases
   - Register it: `subrosa fact upsert --leaf <file.md> --hook "<one-line hook, under ~150 chars>"`. Type and title come from the leaf frontmatter; pass `--pin` for a guardrail that must always load regardless of budget. New facts append to the curated order; updates keep their place.
   - Link related leaves in the body with `[[slug]]` — the other leaf's `name`. A `[[slug]]` naming a leaf you haven't written yet is fine. After registering, run `subrosa fact link <slug>` to check the links resolve; anything shown `[dangling]` is a typo or a leaf still to write.

5. **Convert relative dates to absolute** before writing. "Yesterday", "last Thursday", "next sprint" all rot fast — use today's date as the anchor and write the absolute date.

6. **Use the project-scoped memory directory** from the system prompt's auto memory section (the one rooted at `~/.claude/projects/<sanitized-cwd>/memory/`). Do not create a new location. Facts are keyed by that project.

7. **Staleness review pass.** After writing your saves, scan the active facts (`subrosa fact list`) for entries that may now be obsolete. Lightweight heuristics:
   - Hook contains an absolute date earlier than today (e.g., "Next check: 2026-04-27" once today is past 2026-04-27)
   - Hook contains "Complete", "Done", "Closed", or similar terminal language for work that may now be archivable
   - Hook references a ticket (say, `PROJ-123`) the current conversation indicates is now closed or superseded

   Split candidates into two tiers — auto-archive the high-confidence ones, flag the rest.

   **HIGH-confidence stale → auto-archive (soft-delete).** Both conditions must hold:
   - The leaf file explicitly confirms the terminal state — a `Status: Closed/Done/Resolved/Archived/Cancelled` line, a heading like `## Outcome` ending in completion, or a closing paragraph stating the work is finished/superseded.
   - AND the hook carries a terminal marker (`Complete|Done|Closed|Resolved|Archived|Cancelled|Superseded`) OR the hook's absolute date is more than 30 days past today.

   For each high-confidence match: `subrosa fact archive --leaf <file.md>`. This sets `status='archived'` so the fact drops out of the generated `MEMORY.md`, but the row stays in the DB and the leaf stays on disk — nothing is deleted. List under `Archived (N)` in the report with a one-line reason (which signal in leaf + which signal in hook). To bring one back: `subrosa fact upsert --leaf <file.md>`.

   **LOW-confidence stale → flag only.** Only one signal present, or the leaf doesn't confirm, or you're unsure. List under `Potentially stale (review)` so the user decides. Cap this flag list at ~5 entries — if more surface, suggest the user batch-review staleness in a separate session.

   Keep the scan cheap: read the hook text first. Only open the leaf when a high-confidence archive is on the table; don't open every memory file just to flag.

8. **Regenerate `MEMORY.md`.** Always run, even when nothing was saved this session: `subrosa generate`. The generator rebuilds the index from the active facts, byte-budgeted to 23000 by default, so it can never overflow and no hook ever needs hand-trimming. It ranks by pinned > type weight > recency > hits, keeps the curated order for display, and logs any facts that fell below the budget — those stay in the DB and are still `subrosa search`-able, just not always-loaded. If a dropped fact should always load, `subrosa fact pin --leaf <file.md>` and regenerate. A project that keeps dropping facts can raise its own budget: `echo 24500 > <memdir>/.budget`. Claude Code stops reading `MEMORY.md` at around 25,000 bytes or line 200, so that is the practical ceiling — past it the extra bytes are written but never loaded.

   Never hand-edit `MEMORY.md`; it is overwritten on every regenerate. The generator owns the byte budget — there is no manual hook-trimming step.

9. **Mark this session done, then clear the queue.** First run `subrosa checkpoint-mark` — it stamps the current (live) session's checkpoint high-water mark so it won't re-queue on the next `SessionEnd` unless it grows past this point (without it, the session you just saved pops back into the queue when it ends). Then run `subrosa checkpoint-clear` to empty the pending queue (the `SessionEnd` hook appends a line for every ended session, and the start-of-session nudge counts those; running this skill *is* being caught up, so clearing resets the counter). Order: mark first, then clear.

## Report format

A headline count, a short recap of what the session did (like `/recap` — the work,
not which leaf files changed), then the safe-to-wipe line. Nothing else — no
per-category Saved/Updated/Skipped lists, no byte dumps, no named archive/review sections.

```
✅ Saved 2, Updated 1.

📋 Recap:
- Bumped Rust deps and shipped Dependabot + a monthly toolchain-bump workflow (PR #1, merged)
- Reviewed and merged 5 Dependabot action bumps; confirmed the toolchain cron no-ops cleanly
- Verified CI green across the board

👍 Safe to /clear or /compact.
```

- **Headline** — `✅ Saved <X>, Updated <Y>.` (both counts always shown; leading ✅,
  trailing period). Append `, Archived <Z>` and/or `, Flagged <W>` before the period,
  same comma style, only when non-zero. When nothing was saved or updated (all zero),
  use `✅ Nothing new to save.` instead.
- **Recap** — `📋 Recap:` then 2–5 short bullets on what the session actually
  accomplished (reads like `/recap`), NOT a list of which facts were saved. Always
  show it, even in the zero case.
- **Safe line** — `👍 Safe to /clear or /compact.`
- Still run the full procedure above (save, update, staleness pass, regenerate, mark,
  clear) — this changes only what you print, not what you do.

## Notes

- Don't save just because time was spent on a topic. Save what would be useful to a future-you starting fresh — not a transcript of what just happened.
- If the user explicitly asked you to remember something earlier in this conversation and you haven't saved it yet, do that first.
- If a candidate is borderline, lean toward skipping. The generated `MEMORY.md` is budgeted, so low-signal facts push high-signal ones below the budget — that crowding-out is a real cost.
- Surprising > comprehensive. A short, specific entry is worth more than a thorough one that paraphrases what's already in the codebase or `CLAUDE.md`.
