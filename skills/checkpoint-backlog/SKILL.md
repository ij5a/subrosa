---
name: checkpoint-backlog
description: Checkpoint the ended Claude Code sessions waiting in subrosa's queue. Reads the pending-checkpoint queue and, for each queued past session, extracts durable facts into that project's memory (leaf files + facts database + regenerated MEMORY.md), then clears it from the queue. Run it at session start when there's a backlog; it's also fine to invoke by hand.
---

# checkpoint-backlog — checkpoint the queued (ended) sessions

When a session ends, subrosa's `SessionEnd` hook queues it in `pending-checkpoint.log` (in the subrosa data dir, `~/.claude/subrosa/` by default) for checkpointing. This skill processes that backlog now, **in-session** — no background daemon, no headless `claude` run. It's the same memory procedure as the checkpoint skill, applied to each *past* session instead of the current conversation.

Follow the checkpoint skill's rules (the four memory types — user / feedback / project / reference — the exclusion list, and the leaf → `subrosa fact upsert` → `subrosa generate` flow). Read `${CLAUDE_PLUGIN_ROOT}/skills/checkpoint/SKILL.md` if you need the detail. Apply these overrides.

## Procedure

1. **List the backlog:** `subrosa pending`. Each line is `<timestamp>\t<session-id>`. Collect the unique ids.
   If it's empty, say "no backlog" and stop.

2. **Cap the work** at the 5 most recent queued sessions (newest is last in the file). If there are more,
   say so and tell the user to run `/subrosa:checkpoint-backlog` again. This keeps session startup from stalling.

3. **For each session id, oldest first:**
   - `subrosa session <id>` — prints the flattened turns plus a `# memdir:` line. If it errors with "no archived
     turns" (never ingested), just `subrosa checkpoint-drop <id>` and move on — there's nothing to extract.
   - Read the turns and pull out durable facts per the checkpoint skill's rules, applying the exclusion list strictly.
   - **Be conservative.** This writes straight into the always-loaded `MEMORY.md` with no human review, so when
     a candidate is borderline, skip it. The index is byte-budgeted — low-signal facts crowd out good ones.
   - Check existing memory first: `subrosa fact list --memdir "<memdir>"` and `subrosa search "<keyword>"`. Update a
     fact in place rather than create a near-duplicate.
   - Write each leaf into the `<memdir>` from the dump, then register it:
     `subrosa fact upsert --memdir "<memdir>" --leaf <name>.md --hook "<one-line hook>" --origin-session <id>`
   - Rebuild that project's index: `subrosa generate --memdir "<memdir>"`
   - Clear it from the queue: `subrosa checkpoint-drop <id>`

4. **Do NOT** run `subrosa checkpoint-clear` (it wipes the whole queue, including sessions you didn't reach), and
   do NOT run the staleness archive pass — both stay with the interactive checkpoint skill.

## Report

Keep it short so it doesn't bury the user's actual task:

```
[checkpoint-backlog] N sessions → saved X, updated Y, skipped Z. Queue: M left.
```

If you ran this at session start, finish it, then turn straight to whatever the user asked for.
