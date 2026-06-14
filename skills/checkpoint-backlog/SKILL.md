---
name: checkpoint-backlog
description: Checkpoint the ended Claude Code sessions waiting in subrosa's queue. Reads the pending-checkpoint queue and, for each queued past session, extracts durable facts into that project's memory (leaf files + facts database + regenerated MEMORY.md), then clears it from the queue. Run it at session start when there's a backlog; it's also fine to invoke by hand.
---

# checkpoint-backlog — checkpoint the queued (ended) sessions

When a session ends, subrosa's `SessionEnd` hook queues it in `pending-checkpoint.log` (in the subrosa data dir, `~/.claude/subrosa/` by default) for checkpointing. This skill processes that backlog now, **in-session** — no background daemon, no headless `claude` run. It's the same memory procedure as the checkpoint skill, applied to each *past* session instead of the current conversation.

Follow the checkpoint skill's rules (the four memory types — user / feedback / project / reference — the exclusion list, and the leaf → `subrosa fact upsert` → `subrosa generate` flow). Read `${CLAUDE_PLUGIN_ROOT}/skills/checkpoint/SKILL.md` if you need the detail. Apply these overrides.

When the queue spans **more than one project**, the slow part — reading each session dump to judge what's durable — runs in parallel, one sub-agent per project. A single-project queue stays sequential and behaves exactly as it always has. The split is safe because each project has its own `MEMORY.md`: separate projects never race, but two sessions in the *same* project would race the regenerate and the dedup, so same-project work always stays serial.

## Procedure

1. **List the backlog:** `subrosa pending`. Each line is `<timestamp>\t<session-id>`, oldest first. Collect the unique ids.
   If it's empty, say "no backlog" and stop.

2. **Cap the work** at the 5 most recent queued sessions (newest is last in the file). If there are more,
   process those 5 and tell the user to run `/subrosa:checkpoint-backlog` again for the rest. This keeps session startup from stalling.

3. **Find each session's project (cheap probe).** For each id, run `subrosa session <id> | head -2`. The pipe stops the
   command after the two header lines, so this never dumps the whole session — it works on any recent binary:
   - line 1 — `# session <id>  project=<project>  cwd=<cwd>  <first>..<last>` → take the `project=` value.
   - line 2 — `# memdir: <path>` → take the memdir path.

   If instead it prints `[subrosa] no archived turns for session <id>` (the session was never ingested), there is nothing
   to extract: run `subrosa checkpoint-drop <id>` now and count it as a skip. Don't pass it to a lane.

4. **Group the surviving ids by project, then pick a branch.** If none survive, skip to the report.

   ### Branch A — one project (the common case)
   Process the sessions one at a time, inline in this conversation — byte-identical to the old behavior. For each id, oldest first:
   - `subrosa session <id>` — read the flattened turns.
   - Pull durable facts per the checkpoint skill's rules (four types, exclusion list, be conservative — a borderline
     candidate is a skip, because `MEMORY.md` is byte-budgeted and low-signal facts crowd out good ones).
   - Check existing memory first: `subrosa fact list --memdir "<memdir>"` and `subrosa search "<keyword>"`. Update a fact
     in place rather than create a near-duplicate.
   - Write each leaf into the `<memdir>` from the probe, then register it:
     `subrosa fact upsert --memdir "<memdir>" --leaf <name>.md --hook "<one-line hook>" --origin-session <id>`
   - Rebuild that project's index: `subrosa generate --memdir "<memdir>"`
   - Clear it from the queue: `subrosa checkpoint-drop <id>`

   ### Branch B — more than one project (parallel)
   Fan out **one sub-agent per project**, launched in a single batch with the Agent tool (general-purpose type) so they
   run at the same time. Parallelize *across* projects; keep each project's sessions *serial* inside its own lane.
   - For each project group, launch one sub-agent with the **sub-agent prompt** below, filling in `{{PROJECT}}`,
     `{{MEMDIR}}`, `{{SESSION_IDS}}` (that project's ids, oldest first, space-separated), and `{{PLUGIN_ROOT}}`
     (the real value of `${CLAUDE_PLUGIN_ROOT}`).
   - The sub-agents read, extract, and write facts for their own project only. They do **not** drop anything from the
     queue and never touch another project's memdir.

5. **Drop the queue entries yourself — Branch B only, after every sub-agent has returned.** Each sub-agent reports the
   ids it finished (`FINISHED_IDS:`). For every finished id, run `subrosa checkpoint-drop <id>` one at a time, here in the
   orchestrator — never inside a sub-agent — so the shared `pending-checkpoint.log` has a single writer. Any id a sub-agent
   did **not** report (it failed, or found nothing it could finish) stays in the queue and is retried on the next backlog.
   That is safe and self-healing: `checkpointed_seq` in the database is the real done-marker, the log is just a to-do list.

6. **Do NOT** run `subrosa checkpoint-clear` (it wipes the whole queue, including sessions you didn't reach), and
   do NOT run the staleness archive pass — both stay with the interactive checkpoint skill.

## Sub-agent prompt (Branch B)

Launch one sub-agent per project, all in one batch. Use this exact prompt, with the four placeholders filled in:

```
You are saving durable memory from one or more ended Claude Code sessions that all belong to ONE project. This is the
same memory procedure as subrosa's `checkpoint` skill, applied to past sessions. Read
{{PLUGIN_ROOT}}/skills/checkpoint/SKILL.md if you want the full detail — the load-bearing rules are inlined below.

Project: {{PROJECT}}
Memory directory (memdir): {{MEMDIR}}
Session ids, oldest first: {{SESSION_IDS}}

Process the ids IN ORDER, one at a time. Do not parallelize within this lane — these sessions share one MEMORY.md and
would race each other. For each id:

1. `subrosa session <id>` — read the flattened turns.
2. Pull out durable facts. Match each to one of four types:
   - user — role, preferences, knowledge, working context
   - feedback — corrections ("don't do X") and confirmed-good approaches; always include the why
   - project — ongoing work, deadlines, motivations, who is doing what and why
   - reference — pointers to external systems, dashboards, ticket projects
3. Apply the exclusion list strictly. Do NOT save: code patterns, conventions, file paths, architecture; git history,
   commit hashes, blame, PR numbers; debugging recipes (the fix is in the code, the why is in the commit message);
   anything already in CLAUDE.md; ephemeral state (in-progress task details, the current investigation chain, bare
   ticket numbers without context); routine activity logs. Only save what was surprising or non-obvious.
4. Be conservative. This writes straight into the always-loaded MEMORY.md with no human review, so when a candidate is
   borderline, skip it — the index is byte-budgeted and low-signal facts crowd out good ones.
5. Convert relative dates ("yesterday", "last week", "next sprint") to absolute dates before writing.
6. Check existing memory first so you update instead of duplicating: `subrosa fact list --memdir "{{MEMDIR}}"` and
   `subrosa search "<keyword>"`. Similar fact exists and is correct → skip; exists but stale → update in place; none → new.
7. Write each leaf file into {{MEMDIR}} with frontmatter (name, description, type). For feedback and project facts, use
   the **Why:** / **How to apply:** structure in the body — the why is what lets a future reader judge edge cases. Link
   related leaves with [[their-name]].
8. Register the fact:
   `subrosa fact upsert --memdir "{{MEMDIR}}" --leaf <name>.md --hook "<one-line hook, under ~150 chars>" --origin-session <id>`
9. Rebuild this project's index after EACH session: `subrosa generate --memdir "{{MEMDIR}}"`. Regenerating per session
   (not once at the end) means a failure partway through still leaves the earlier sessions saved.

Hard rules:
- Use ONLY these commands: `subrosa session`, `subrosa fact list`, `subrosa fact upsert`, `subrosa search`, `subrosa generate`.
- NEVER run `subrosa checkpoint-drop` or `subrosa checkpoint-clear` — the orchestrator owns the queue.
- NEVER write to any memdir other than {{MEMDIR}}.
- If `subrosa session <id>` prints "no archived turns", there is nothing to extract — count that id finished and move on.

When done, return EXACTLY this and nothing else:

FINISHED_IDS: <space-separated ids you fully handled: saved, updated, or confirmed nothing-to-save>
PER_SESSION:
<id>: saved <n>, updated <n>, skipped <n> — <very short note>
TOTALS: saved <X>, updated <Y>, skipped <Z>
```

## Report

Add up the work across both branches (the inline Branch A sessions, every sub-agent's `TOTALS`, and the no-turns skips
from step 3). Then re-run `subrosa pending` and count the unique ids left — that is the authoritative `M`, not a guess.
Keep it short so it doesn't bury the user's actual task:

```
[checkpoint-backlog] N sessions → saved X, updated Y, skipped Z. Queue: M left.
```

If you ran this at session start, finish it, then turn straight to whatever the user asked for.
