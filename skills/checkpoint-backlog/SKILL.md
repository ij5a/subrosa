---
name: checkpoint-backlog
description: Checkpoint the ended Claude Code sessions waiting in subrosa's queue. Reads the pending-checkpoint queue and, for each queued past session, extracts durable facts into that project's memory (leaf files + facts database + regenerated MEMORY.md), then clears it from the queue. Run it at session start when there's a backlog; it's also fine to invoke by hand.
---

# checkpoint-backlog: checkpoint queued sessions

When a session ends, subrosa's `SessionEnd` hook adds it to `pending-checkpoint.log` in `~/.claude/subrosa/` by default. This skill processes the queue **in-session**. It uses no background daemon or headless `claude` run. It applies the checkpoint skill to each *past* session.

Follow the checkpoint skill's 4 types, user, feedback, project, and reference rules. Follow its exclusion list and leaf to `subrosa fact upsert` to `subrosa generate` flow. Read `${CLAUDE_PLUGIN_ROOT}/skills/checkpoint/SKILL.md` for details. Apply these overrides.

When the queue spans **more than one project**, read each session dump in parallel, with one sub-agent per project. Keep a single-project queue sequential. Separate projects have separate `MEMORY.md` files and do not race. Sessions in the same project would race regeneration and deduplication, so keep them serial.

## Procedure

1. **List the backlog:** Run `subrosa pending`. Each line is `<timestamp>\t<session-id>`, oldest first. Collect unique ids. If it is empty, say "no backlog" and stop.

2. **Cap the work** at the 5 most recent queued sessions. The newest is last in the file. If more remain, process those 5. Tell the user to run `/subrosa:checkpoint-backlog` again for the rest. This prevents session startup from stalling.

3. **Find each session's project.** For each id, run `subrosa session <id> | head -2`. The pipe stops after 2 header lines. It avoids dumping the whole session and works with any recent binary:
   - Line 1 is `# session <id>  project=<project>  cwd=<cwd>  <first>..<last>`. Take the `project=` value.
   - Line 2 is `# memdir: <path>`. Take the memdir path.

   If it prints `[subrosa] no archived turns for session <id>`, the session was never ingested. There is nothing to extract. Run `subrosa checkpoint-drop <id>`. Count it as a skip. Do not pass it to a lane.

4. **Group surviving ids by project, then choose a branch.** If none survive, skip to the report.

   ### Branch A: one project

   Process sessions one at a time in this conversation. This matches the old byte behavior. Process ids oldest first:
   - Run `subrosa session <id>` and read the flattened turns.
   - Pull durable facts using the checkpoint rules. Use the 4 types and exclusion list. Skip borderline candidates because `MEMORY.md` is byte-budgeted and low-signal facts can hide good ones.
   - Check memory first with `subrosa fact list --memdir "<memdir>"` and `subrosa search "<keyword>"`. Update a similar stale fact instead of creating a duplicate.
   - Write each leaf in the probed `<memdir>`. Register it with `subrosa fact upsert --memdir "<memdir>" --leaf <name>.md --hook "<one-line hook>" --origin-session <id>`.
   - Rebuild the project index with `subrosa generate --memdir "<memdir>"`.
   - Remove it from the queue with `subrosa checkpoint-drop <id>`.

   ### Branch B: more than one project

   Launch **one sub-agent per project** in one batch with the Agent tool (general-purpose type). Run project lanes at the same time. Keep each project's sessions *serial* inside its lane.
   - Use the **sub-agent prompt** below for each group. Fill `{{PROJECT}}`, `{{MEMDIR}}`, `{{SESSION_IDS}}` (oldest first, space-separated), and `{{PLUGIN_ROOT}}` (the real `${CLAUDE_PLUGIN_ROOT}`).
   - Each sub-agent reads, extracts, and writes only its project's facts. It must not drop queue entries or touch another project's memdir.

5. **Drop queue entries yourself, Branch B only, after every sub-agent returns.** Each sub-agent reports finished ids with `FINISHED_IDS:`. Run `subrosa checkpoint-drop <id>` once per finished id, one at a time, in the orchestrator. Never run it inside a sub-agent. This gives shared `pending-checkpoint.log` one writer.

   A failed or unreported id stays queued for the next backlog. This is safe and self-healing. `checkpointed_seq` in the database is the real done-marker. The log is only a to-do list.

6. **Do NOT** run `subrosa checkpoint-clear`. It wipes the whole queue, including unreached sessions. Do **not** run the staleness archive pass. Both actions stay with the interactive checkpoint skill.

## Sub-agent prompt (Branch B)

Launch one sub-agent per project in one batch. Use this prompt with the four placeholders filled in:

```
You are saving durable memory from ended Claude Code sessions in ONE project. Use the `checkpoint` skill procedure for past sessions. Read
{{PLUGIN_ROOT}}/skills/checkpoint/SKILL.md for details. The key rules are below.

Project: {{PROJECT}}
Memory directory (memdir): {{MEMDIR}}
Session ids, oldest first: {{SESSION_IDS}}

Process ids IN ORDER, one at a time. Do not parallelize this lane. Sessions share one MEMORY.md and would race. For each id:

1. `subrosa session <id>` Read the flattened turns.
2. Pull out durable facts. Match each to one type:
   - user: role, preferences, knowledge, working context
   - feedback: corrections ("don't do X") and confirmed-good approaches; always include the why
   - project: ongoing work, deadlines, motivations, who is doing what and why
   - reference: pointers to external systems, dashboards, ticket projects
3. Apply the exclusion list strictly. Do NOT save code patterns, conventions, file paths, architecture, git history, commit hashes, blame, PR numbers, or debugging recipes. The fix is in code. The reason is in the commit message. Do not save anything in CLAUDE.md, ephemeral state, bare ticket numbers without context, or routine activity logs. Only save what was surprising or non-obvious.
4. Be conservative. This writes directly to always-loaded MEMORY.md without human review. Skip borderline candidates because the index is byte-budgeted.
5. Convert relative dates ("yesterday", "last week", "next sprint") to absolute dates before writing.
6. Check existing memory first: `subrosa fact list --memdir "{{MEMDIR}}"` and `subrosa search "<keyword>"`. Skip a correct similar fact. Update a stale one in place. Create a new one only when needed.
7. Write each leaf into {{MEMDIR}} with frontmatter (name, description, type). For feedback and project facts, use **Why:** and **How to apply:**. Include why for edge cases. Link related leaves with [[their-name]].
8. Register the fact:
   `subrosa fact upsert --memdir "{{MEMDIR}}" --leaf <name>.md --hook "<one-line hook, under ~150 chars>" --origin-session <id>`
9. Rebuild this project's index after EACH session with `subrosa generate --memdir "{{MEMDIR}}"`. Regenerate per session, not once at the end. A partial failure must leave earlier sessions saved.

Hard rules:
- Use ONLY these commands: `subrosa session`, `subrosa fact list`, `subrosa fact upsert`, `subrosa search`, `subrosa generate`.
- NEVER run `subrosa checkpoint-drop` or `subrosa checkpoint-clear`. The orchestrator owns the queue.
- NEVER write to any memdir other than {{MEMDIR}}.
- If `subrosa session <id>` prints "no archived turns", extract nothing. Count that id finished and move on.

When done, return EXACTLY this and nothing else:

FINISHED_IDS: <space-separated ids you fully handled: saved, updated, or confirmed nothing-to-save>
PER_SESSION:
<id>: saved <n>, updated <n>, skipped <n>; <very short note>
TOTALS: saved <X>, updated <Y>, skipped <Z>
```

## Report

Add the work from inline Branch A sessions, each sub-agent's `TOTALS`, and no-turns skips from step 3. Re-run `subrosa pending`. Count unique ids left. That is the authoritative `M`.

Keep the report short:

```
[checkpoint-backlog] N sessions → saved X, updated Y, skipped Z. Queue: M left.
```

If you ran this at session start, finish it, then return to the user's task.
