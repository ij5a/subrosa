# How subrosa compares

This table summarizes public documentation as of 2026-06-13. It describes each design and is not a runtime test. The links support the claims.

| | subrosa | claude-mem | Remember (Anthropic) | memsearch | claude-supermemory |
|---|---|---|---|---|---|
| What it is | Local session archive with selective recall | Memory compression, semantic search, and web UI | Official plugin with tiered daily logs | Markdown memory with local vector search | Plugin for Supermemory cloud |
| Runtime | One static 5 MB binary with no daemon | TypeScript and Bun worker on port 37777 [^cm-readme] | Plugin using Claude Haiku to process saves | Python and Milvus Lite | Node plugin and cloud API |
| Storage | Local SQLite database | Local SQLite and vector store | Local logs | Local Markdown and Milvus Lite, with cloud opt-in | Supermemory cloud [^sm-docs] |
| Save cost | `0`, mechanical parsing, no LLM call | Claude Agent SDK processes each session [^cm-arch] | Usually less than $0.01 per save [^rem] | Local embeddings, no LLM call | Cloud processing and Supermemory Pro at $19 per month or more [^sm-price] |
| Context | About 180 tokens per prompt, usually 0. `MEMORY.md` is capped at 23 KB by default. | Worker-managed context | Tiered logs at session start | Documents zero extra tokens for automatic injection [^ms-blog] | Saved memories at session start [^sm-docs] |
| Recall | Keyword-only automatic recall with word roots, exact hyphenated IDs, centered snippets, recency ties, project scope, and per-session deduplication. `--fuzzy` adds trigrams. Semantic search is direct with `--semantic` or automatic after a zero-hit plain search when the local model and index exist. `--raw` skips that retry. | Keyword and semantic search | Log reload | Semantic vector search | Cloud semantic search |
| Redaction | Keys, tokens, and labeled secret values are masked before storage | Manual `<private>` tags | Not documented | Not documented | Not documented in the cited public sources [^sm-redact] |
| Network | The binary opens no sockets except the one-time model download through a system `curl` child. No text is uploaded. | Local worker and SDK calls for summarization | Anthropic API through Haiku | Local by default | Required for each session |
| Price and license | Free, MIT | Free, Apache-2.0 | Free plugin, pay per save | Free, MIT | Free plugin, service at $19 per month or more |
| State on 2026-06-13 | Active | v13.5.6, 82,000 stars, active | Official, active | Active with Zilliz | Last release 2026-02-09 [^sm-repo] |

## Other documented designs

- claude-mem provides semantic search, a web timeline, and capture across Gemini CLI and OpenCode through its worker and Claude summarization flow.
- memsearch stores readable Markdown and local vectors. It also supports a cloud option.
- Remember is the official Anthropic option. It uses tiered logs and charges for session saves.
- claude-supermemory provides shared memory through a cloud service. Its plugin requires Supermemory Pro.

## What subrosa is not

subrosa does not provide multi-user memory. It does not send transcripts to a cloud service. Semantic search stays local after the model download. A plain search can use the local index after a zero-hit exact search, while automatic prompt recall remains keyword-only.

[^cm-readme]: claude-mem README: worker service with an HTTP API on port 37777, managed by Bun: <https://github.com/thedotmack/claude-mem>
[^cm-arch]: claude-mem hooks architecture: Claude Agent SDK summarization: <https://docs.claude-mem.ai/hooks-architecture>
[^rem]: Remember plugin listing: <https://claude.com/plugins/remember>
[^ms-blog]: Milvus blog on memsearch: <https://milvus.io/blog/adding-persistent-memory-to-claude-code-with-the-lightweight-memsearch-plugin.md>
[^sm-docs]: Supermemory Claude Code integration: <https://supermemory.ai/docs/integrations/claude-code>
[^sm-price]: claude-supermemory README and Supermemory pricing: <https://supermemory.ai/pricing>
[^sm-redact]: No public transcript-redaction documentation was found for the cited Supermemory plugin.
[^sm-repo]: <https://github.com/supermemoryai/claude-supermemory>
