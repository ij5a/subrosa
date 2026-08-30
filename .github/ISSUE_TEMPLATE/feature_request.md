---
name: Feature request
about: Suggest an idea or improvement
title: ''
labels: enhancement
assignees: ''
---

## The problem

What is missing or hard to use? What are you trying to do?

## Proposed solution

How should it work?

## Alternatives considered

What other options did you consider?

## Does it fit subrosa's scope?

subrosa has these constraints. Requests that fit them are more likely to land:

- **Private and local-only.** No cloud, telemetry, or socket. Everything stays on your machine. The only exception is the semantic-search model download, which only pulls files down.
- **Small supply chain.** Eleven direct dependencies and one static binary. A new dependency needs a strong reason.
- **Claude Code only.** It archives Claude Code sessions. Other tools (Gemini, OpenCode, …) are out of scope.
- **No server or web dashboard.** The CLI and plugin stay local.

If your idea needs a server, a service to call, or cross-tool support, it is probably out of scope. You may still open it for discussion.
