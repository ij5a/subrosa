# How subrosa compares

Persistent-memory options for Claude Code, as of 2026-06-13. This compares what each project **documents** — its own docs, README, or issue tracker, all linked so you can check — not hands-on testing of the other tools. If a claim is wrong or out of date, [open an issue](https://github.com/ij5a/subrosa/issues) and I'll fix it.

| | subrosa | claude-mem | Remember (Anthropic) | memsearch | claude-supermemory |
|---|---|---|---|---|---|
| What it is | Saves every session locally + selective recall | Memory compression, meaning-based search, web UI | Official plugin: tiered daily logs | Markdown memory + local vector search | Bridge to the Supermemory cloud |
| Runtime | One 3.7 MB static binary, no daemon | TypeScript + Bun worker on port 37777 [^cm-readme] | Plugin (uses Claude Haiku to process saves) | Python + Milvus Lite | Node plugin + cloud API |
| Storage | Local SQLite (full-text search) | Local SQLite + vector store | Local logs | Local Markdown + Milvus Lite (cloud opt-in) | Supermemory cloud [^sm-docs] |
| Cost to save a session | Zero — mechanical parsing, no LLM call | Runs each session through Claude (Agent SDK) [^cm-arch] | "Typical cost is less than $0.01 per session save." [^rem] | Builds embeddings locally, no LLM call | Cloud processing; plugin requires Supermemory Pro ($19/mo+) [^sm-price] |
| Context injected | ≤ ~180 tokens per prompt, usually 0; index hard-capped at 23 KB | Some users report heavy token use at session start [^cm-618] [^cm-1848] | Reloads tiered logs at session start | "Automatic Context Injection Costs Zero Extra Tokens" [^ms-blog] | Injects saved memories at session start [^sm-docs] |
| Recall | Keyword search, word-root matching, hyphen-safe identifiers, match-centered snippets, recency tie-break, + opt-in fuzzy (trigram), project-scoped, deduped per session | Keyword + meaning-based | Log reload | Meaning-based (vector) | Cloud, meaning-based |
| Secret redaction before storage | Yes — keys, tokens, `password=` values masked at write | Manual `<private>` tags | Not documented | Not documented | Not documented either way [^sm-redact] |
| Network calls | None from the binary, ever | Local worker; SDK calls for summarization | Anthropic API (Haiku) | Local by default | Required, every session |
| Price / license | Free, MIT | Free, Apache-2.0 | Free plugin, pay per save | Free, MIT | Plugin free, service $19/mo+ |
| State (2026-06-13) | Active | v13.5.6, 82k stars, very active | Official, active | Active (Zilliz) | Last release 2026-02-09 [^sm-repo] |

## When something else is the better choice

- **claude-mem** if you want meaning-based search, a web timeline view, or capture across Gemini CLI and OpenCode — and you're fine with a background worker that summarizes each session through Claude. It's popular for a reason.
- **memsearch** if you want local meaning-based search and Markdown files you can read directly.
- **Remember** if you want the official Anthropic option with zero setup and accept the per-save cost.
- **claude-supermemory** if team-shared memory is the point — that's a cloud-shaped feature subrosa deliberately doesn't have.

What subrosa won't do: meaning-based search with embeddings (keyword search is the deliberate trade for a 5-crate static binary and zero saving cost), multi-user memory, or anything that needs your transcripts to leave your machine.

[^cm-readme]: claude-mem README — "Worker Service - HTTP API on port 37777 … managed by Bun": <https://github.com/thedotmack/claude-mem>
[^cm-arch]: claude-mem hooks architecture — "Sends to Claude Agent SDK for summarization": <https://docs.claude-mem.ai/hooks-architecture>
[^cm-618]: claude-mem issue #618 "Uses too much tokens" — "claude code consumes all my tokens in <10 messages": <https://github.com/thedotmack/claude-mem/issues/618>
[^cm-1848]: claude-mem issue #1848 "token consumption" — "The moment I start the session, 40% of my tokens disappear instantly": <https://github.com/thedotmack/claude-mem/issues/1848>
[^rem]: Remember plugin listing: <https://claude.com/plugins/remember>
[^ms-blog]: Milvus blog on memsearch: <https://milvus.io/blog/adding-persistent-memory-to-claude-code-with-the-lightweight-memsearch-plugin.md>
[^sm-docs]: Supermemory Claude Code integration: <https://supermemory.ai/docs/integrations/claude-code>
[^sm-price]: claude-supermemory README — "Requires Supermemory Pro or above"; pricing: <https://supermemory.ai/pricing>
[^sm-redact]: We could not find any public documentation on transcript redaction for the Supermemory plugin — listed as undocumented, not as absent.
[^sm-repo]: <https://github.com/supermemoryai/claude-supermemory>
