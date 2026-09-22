---
name: checkpoint-backlog
description: Check ended Claude Code sessions in subrosa's queue. Read the SQLite queue, save facts, regenerate MEMORY.md, and clear completed entries. Run at session start or by hand.
---

# checkpoint-backlog: checkpoint queued sessions

When a session ends, `SessionEnd` adds it to the database queue. Older versions used `~/.claude/subrosa/pending-checkpoint.log`. The queue may drain automatically when `distill` is configured; this skill processes anything still queued **in-session**. It applies the checkpoint skill to each *past* session.

Follow the checkpoint skill's 4 types, rules, exclusions, and `subrosa fact upsert` to `subrosa generate` flow. Read `${CLAUDE_PLUGIN_ROOT}/skills/checkpoint/SKILL.md`. Apply these overrides.

For **more than one project**, read dumps in parallel, one sub-agent per project. Keep one project sequential. Separate `MEMORY.md` files prevent races; same-project sessions would race regeneration and deduplication.

## Procedure

1. **List the backlog:** Run `subrosa pending`. Each line is `<timestamp>\t<session-id>`, newest first. Collect unique ids. If empty, say "no backlog" and stop.

2. **Process all queued sessions.** Work through every id from `subrosa pending`, newest first. Process every queued id. Drop each completed session or leave it queued explicitly.

3. **Find each session's project.** For each id, run `subrosa session <id> | head -2`. The pipe stops after 2 header lines. It avoids dumping the whole session and works with any recent binary:
   - Line 1 is `# session <id>  project=<project>  cwd=<cwd>  <first>..<last>`. Take `project=`.
   - Line 2 is `# memdir: <path>`. Take the memdir. Run `subrosa ingest --require-complete <transcript>` with the session's live transcript path. A non-zero exit means UNKNOWN: report why, leave the id queued, and do not mark or drop it or pass it to a lane. If it exits 0, run `subrosa session <id> --boundary | head -3` and store `# last_seq: <DISTILLED_SEQ>`, the inclusive boundary. Only a complete ingest plus a boundary reporting no archived turns permits `subrosa checkpoint-drop <id> --max-seq=-1` and a counted skip. Do not pass it to a lane.

   If it prints `[subrosa] no archived turns for session <id>`, only a prior complete ingest that exited 0 permits the empty result. Drop it with `subrosa checkpoint-drop <id> --max-seq=-1` and count a skip. A failed or incomplete ingest stays queued.

4. **Group surviving ids by project, then choose a branch.** If none survive, report.

   ### Branch A: one project

   Process sessions one at a time. This matches old byte behavior. Process ids newest first:
   - Run `subrosa session <id>` and read its flattened turns.
   - Pull durable facts using the checkpoint rules, 4 types, and exclusion list. Skip borderline candidates because `MEMORY.md` is byte-budgeted and low-signal facts can hide good ones.
   - Check memory with `subrosa fact list --memdir "<memdir>"` and `subrosa search "<keyword>"`. Update a similar stale fact instead of duplicating it.
   - Write each leaf in the probed `<memdir>`. Register it with `subrosa fact upsert --memdir "<memdir>" --leaf <name>.md --hook "<one-line hook>" --origin-session <id>`.
   - Rebuild the index with `subrosa generate --memdir "<memdir>"`.
   - Before removing it, run `subrosa ingest --require-complete <transcript>` again. A non-zero exit reports UNKNOWN and leaves the id queued. If it exits 0, run `subrosa session <id> --boundary | head -3`. If `last_seq` is greater than `DISTILLED_SEQ`, leave it queued and report that it grew. Otherwise remove it with `subrosa checkpoint-drop <id> --max-seq <DISTILLED_SEQ>` only after handling all archived turns through `<DISTILLED_SEQ>`. Older versions used `subrosa checkpoint-drop <id>`.

   ### Branch B: more than one project

   Launch **one sub-agent per project** in one batch with the Agent tool (general-purpose type). Run lanes together; keep sessions *serial*.
   - Use the **sub-agent prompt** below. Fill `{{PROJECT}}`, `{{MEMDIR}}`, `{{SESSION_IDS}}` (newest first, space-separated), and `{{PLUGIN_ROOT}}` (the real `${CLAUDE_PLUGIN_ROOT}`).
   - Each sub-agent writes only its project's facts. It must not drop queue entries or touch another memdir.

5. **Drop queue entries yourself, Branch B only, after sub-agents return.** Each reports finished ids and `DISTILLED_SEQ` boundaries, or reports an id as `empty; safe to drop` after ingest. Run `subrosa checkpoint-drop <id> --max-seq <DISTILLED_SEQ>` for finished ids and `subrosa checkpoint-drop <id> --max-seq=-1` for ids reported `empty; safe to drop`. Never run it inside a sub-agent.

   A failed or unreported id stays queued. This is safe and self-healing. `checkpointed_seq` is the database done-marker.

6. **Do NOT** run `subrosa checkpoint-clear`. It refuses while any queue entry remains. Do **not** run the staleness archive pass.

## Sub-agent prompt

Launch one sub-agent per project. Fill the 4 placeholders:

```
You are saving durable memory from ended Claude Code sessions in ONE project. Use the `checkpoint` skill procedure for past sessions. Read
{{PLUGIN_ROOT}}/skills/checkpoint/SKILL.md for details. The key rules are below.

Project: {{PROJECT}}
Memory directory (memdir): {{MEMDIR}}
Session ids, newest first: {{SESSION_IDS}}

Process ids IN ORDER, one at a time. Do not parallelize this lane. Sessions share one MEMORY.md and would race. For each id:

1. Run `subrosa ingest --require-complete <transcript>`. A non-zero exit is UNKNOWN: report why and leave the id queued. After an exit 0, run `subrosa session <id> --boundary` to capture the sequence boundary before reading the flattened turns.
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
- Use ONLY these commands: `subrosa ingest`, `subrosa session`, `subrosa fact list`, `subrosa fact upsert`, `subrosa search`, `subrosa generate`.
- NEVER run `subrosa checkpoint-drop` or `subrosa checkpoint-clear`. The orchestrator owns the queue.
- NEVER write to any memdir other than {{MEMDIR}}.
- If `subrosa session <id>` prints "no archived turns" after a complete ingest exit 0, extract nothing and report `empty; safe to drop`. The orchestrator drops it with `subrosa checkpoint-drop <id> --max-seq=-1`. A failed or incomplete ingest is a failure report, not an empty result.

Before reporting an id finished, run `subrosa ingest --require-complete <transcript>` again. If it fails, leave the id queued and report UNKNOWN. If it exits 0, re-read the boundary. If it grew past the captured boundary, leave it queued and do not report it as finished.

If a session has a permanently malformed line, its stored incomplete state must be cleared by hand only after the source is fixed or accepted: `sqlite3 "$SUBROSA_DIR/memory.db" "DELETE FROM turns WHERE session_id='<id>'; UPDATE sessions SET num_turns=0, last_seq=-1, file_size=0, file_mtime=0, skipped_lines=0, partial_tail=0, scan_offset=0, scan_seq=0 WHERE session_id='<id>';"`. The cursor reset makes the next check read the repaired line. Run `subrosa ingest --require-complete` again before clearing its queue entry.

This discards that session's archived turns and re-reads the file from the start. If a transcript is rewritten instead of appended to, subrosa refuses it, keeps archived turns intact, and leaves the session queued. Check the file, apply the SQLite repair above, then run the complete ingest check again.

When done, return EXACTLY this and nothing else:

`FINISHED_IDS:` <space-separated ids you fully handled: saved, updated, or confirmed nothing-to-save>
PER_SESSION:
<id>: boundary <DISTILLED_SEQ>; saved <n>, updated <n>, skipped <n>; <very short note>
TOTALS: saved <X>, updated <Y>, skipped <Z>
```

## Report

Print `👍 Safe to /clear or /compact.` only for a session that was marked. For a session that grew, print `⚠️ Session grew; do not /clear or /compact.` For a session with genuinely no archived turns after ingest, print `⚠️ No archived turns; do not /clear or /compact.`

Add Branch A work, each `TOTALS`, and no-turn skips. Re-run `subrosa pending`. Count ids left. `M` is authoritative.

Keep it short:

```
[checkpoint-backlog] N sessions → saved X, updated Y, skipped Z. Queue: M left.
```

At session start, finish, then return to the user's task.
