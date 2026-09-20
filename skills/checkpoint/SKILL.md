---
name: checkpoint
description: Flush durable facts from the current conversation into subrosa's memory before /clear or /compact. Scans the session for user, feedback, project, and reference items; writes or updates leaf memory files and the facts database, then regenerates the byte-budgeted MEMORY.md; soft-archives stale entries; reports a short session recap and confirms it's safe to wipe.
---

# checkpoint: flush memory before a context wipe

The user is about to run `/clear` or `/compact`. Save durable facts before either command, then report whether it is safe to wipe.

`MEMORY.md` is **generated** from (`~/.claude/subrosa/memory.db`). Never hand-edit it. Write leaf files and register facts. The generator rebuilds the byte-budgeted index. Use the `subrosa` CLI (`subrosa fact …`, `subrosa generate`, `subrosa search …`). If it is not on PATH, bootstrap installs it at `~/.claude/subrosa/bin/subrosa`.

## Procedure

1. **Scan the full conversation.** Review every turn, not only the last. Classify each candidate as one of these four types:
   - **user:** role, preferences, knowledge, and working context
   - **feedback:** corrections and validated approaches. Include the reason for both.
   - **project:** ongoing work, deadlines, motivations, roles, and reasons
   - **reference:** pointers to external systems, dashboards, and ticket projects

2. **Apply the exclusion list strictly.** Never save:
   - Code patterns, conventions, file paths, or architecture
   - Git history, commit hashes, blame info, or PR numbers
   - Debugging recipes. The fix is in code, and the reason is in the commit message.
   - Anything already covered by `CLAUDE.md`
   - Ephemeral state, such as task details, investigation chains, or bare ticket numbers
   - Routine activity logs. Save only what was *surprising* or *non-obvious*.

   These exclusions apply even when the user says, "save this." Ask what was non-obvious or whether they meant `CLAUDE.md`.

3. **Check existing memory before writing.** Run `subrosa fact list` for curated facts. Run `subrosa search "<keyword>"` for the full transcript archive. Read related leaf files. For each candidate:
   - A correct similar fact exists: skip it.
   - A stale similar fact exists: update it in place.
   - No similar fact exists: write a new one.

4. **Write the leaf, then register the fact. Do NOT hand-edit `MEMORY.md`.**
   - Create or update the leaf with frontmatter: `name`, `description`, `type`.
   - Use **Why:** and **How to apply:** for `feedback` and `project`. The why helps future readers judge edge cases.
   - Register it: `subrosa fact upsert --leaf <file.md> --hook "<one-line hook, under ~150 chars>"`. Type and title come from frontmatter. Pass `--pin` for an always-loaded guardrail, but pinned facts still compete for the byte budget. On update, the stored title stays unless you pass `--title`. New facts append; updates keep their place.
   - Link related leaves with `[[slug]]`, using the other leaf's `name`. A not-yet-written leaf is allowed. Run `subrosa fact link <slug>` after registration. `[dangling]` means a typo or missing leaf.

5. **Convert relative dates to absolute dates** before writing. Use today's date as the anchor. For example, replace "yesterday", "last Thursday", or "next sprint".

6. **Use the project-scoped memory directory** from the system prompt's auto memory section, rooted at `~/.claude/projects/<sanitized-cwd>/memory/`. Do not create another location. Facts use that project key.

7. **Review stale facts.** After saving, scan active facts with `subrosa fact list`. Use these signals:
   - The hook has an absolute date before today, such as "Next check: 2026-04-27" after that date.
   - The hook has "Complete", "Done", "Closed", or similar terminal language for possibly archivable work.
   - The hook names a ticket, such as `PROJ-123`, that this conversation says is closed or superseded.

   Split candidates into high-confidence and low-confidence tiers. Auto-archive the first tier. Flag the second.

   **HIGH-confidence stale: auto-archive (soft-delete).** Both conditions must hold:
   - The leaf confirms the terminal state. It may use `Status: Closed/Done/Resolved/Archived/Cancelled`, a heading like `## Outcome` ending in completion, or a closing paragraph that says the work is finished or superseded.
   - The hook has a terminal marker (`Complete|Done|Closed|Resolved|Archived|Cancelled|Superseded`) or an absolute date more than 30 days past today.

   For each match, run `subrosa fact archive --leaf <file.md>`. This sets `status='archived'`, so the fact leaves generated `MEMORY.md`. The DB row and leaf stay on disk. Nothing is deleted. Count it as `Archived <Z>` in the headline. Restore it with `subrosa fact upsert --leaf <file.md>`.

   **LOW-confidence stale flags: flag only.** Use this tier when one signal exists, the leaf does not confirm it, or you are unsure. Count it as `Flagged <W>` in the headline. Cap the stale-flag list at ~5 entries. If more appear, suggest a separate batch review.

   Keep the scan cheap. Read hook text first. Open a leaf only for a possible high-confidence archive. Do not open every memory file just to flag it.

8. **Regenerate `MEMORY.md`.** Always run `subrosa generate`, even when nothing was saved. The generator rebuilds active facts with a 23000-byte default budget. It prevents overflow and removes manual hook trimming.

   It ranks by pinned > type weight > recency > hits and keeps curated display order. It logs facts below budget. They stay in the DB and remain `subrosa fact search`-able, but are not always loaded.

   Pin a dropped fact with `subrosa fact pin --leaf <file.md>`, then regenerate. Raise the project budget with `echo 24500 > <memdir>/.budget`. Claude Code stops reading `MEMORY.md` at around 25,000 bytes or line 200. That is the practical ceiling. Extra bytes are written but not loaded.

   Never hand-edit `MEMORY.md`. Regeneration overwrites it and owns the byte budget. No manual hook trimming exists.

9. **Lint what you wrote.** Run `subrosa fact doctor`. It is read-only. It exits 1 for broken or spliced frontmatter, missing `name`/`description`/`type`, duplicate active name slugs, or fact rows whose leaf files are gone.

   A collision with only an archived fact is a warning. Warnings exit 0 for an unregistered leaf, unknown type, or dangling `[[link]]`.

   Run it after writing leaves. Fix errors on touched leaves. The command never edits leaves. Handle older findings separately.

10. **Capture the boundary and mark this session done.** Obtain the current session id, `<id>`, and transcript path, `<transcript>`, from the session environment. Run `subrosa ingest --require-complete <transcript>`. A non-zero exit means UNKNOWN: leave the session queued, report why, and never mark or drop it. Only a complete ingest plus a boundary reporting no archived turns allows `subrosa checkpoint-drop <id> --max-seq=-1`. Otherwise run `subrosa session <id> --boundary` and store its `# last_seq: <DISTILLED_SEQ>` value. Read and distill only through that sequence. Before removal, run `subrosa ingest --require-complete <transcript>` again. If it fails, leave the session queued. Then run the boundary command again and compare its `last_seq` with `DISTILLED_SEQ`. If it has grown, say the session stayed queued, and do not mark or drop it. Otherwise run `subrosa checkpoint-mark <id> --max-seq <DISTILLED_SEQ>`; older versions used `subrosa checkpoint-mark` without the boundary. If a queue entry has to go, run `subrosa checkpoint-drop <id> --max-seq <DISTILLED_SEQ>` after that session's facts are verified on disk. Without `--max-seq`, `checkpoint-drop` uses the distilled watermark. Do not run `subrosa checkpoint-clear`; it refuses while any queue entry remains.

If a session has a permanently malformed line, `--require-complete` keeps failing because the unread count is stored. After fixing or accepting the source by hand, clear its stored state and reset the cursor so the repaired content is read: `sqlite3 "$SUBROSA_DIR/memory.db" "DELETE FROM turns WHERE session_id='<id>'; UPDATE sessions SET num_turns=0, last_seq=-1, file_size=0, file_mtime=0, skipped_lines=0, partial_tail=0, scan_offset=0, scan_seq=0 WHERE session_id='<id>';"`, then run the complete ingest check again. Do not drop the queue entry before that check succeeds.

This discards that session's archived turns and re-reads the file from the start. If a transcript is rewritten instead of appended to, subrosa refuses it, keeps archived turns intact, and leaves the session queued. Check the file, apply the SQLite repair above, then run the complete ingest check again.

## Report format

Print a headline, a recap with 2 to 5 short bullets, and a safe-to-wipe line. Print nothing else. Do not print byte dumps.

```
✅ Saved 2, Updated 1.

📋 Recap:
- Bumped Rust deps and shipped Dependabot + a monthly toolchain-bump workflow (PR #1, merged)
- Reviewed and merged 5 Dependabot action bumps; confirmed the toolchain cron no-ops cleanly
- Verified CI green across the board

👍 Safe to /clear or /compact.
```

- **Headline:** Use `✅ Saved <X>, Updated <Y>.` with both counts. Keep ✅ and the final period. Add `, Archived <Z>` and/or `, Flagged <W>` only when non-zero. Use `✅ Nothing new to save.` when all 4 counts are zero.
- **Recap:** Print `📋 Recap:` and 2–5 short bullets. Do not list saved facts.
- **Safe line:** Print `👍 Safe to /clear or /compact.` only when the session was marked. If it grew, print `⚠️ Session grew; do not /clear or /compact.` If it had no archived turns, print `⚠️ No archived turns; do not /clear or /compact.`
- Run the full procedure: save, update, staleness pass, regenerate, lint, and mark. Change only the report. Do not clear the queue.

## Notes

- Do not save a topic only because it took time. Save what helps a future reader start fresh.
- If the user asked for a fact and you have not saved it, save it first.
- Skip borderline candidates. `MEMORY.md` has a budget, so low-signal facts can hide high-signal facts.
- Prefer surprising facts over summaries. Keep entries short and specific. Do not repeat the codebase or `CLAUDE.md`.
