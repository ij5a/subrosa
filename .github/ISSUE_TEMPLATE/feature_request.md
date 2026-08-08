---
name: Feature request
about: Suggest an idea or improvement
title: ''
labels: enhancement
assignees: ''
---

## The problem

What is missing or awkward today? What are you trying to do?

## Proposed solution

How would you like it to work?

## Alternatives considered

Anything else you thought about.

## Does it fit subrosa's scope?

subrosa is built around a few deliberate constraints. A request that fits these is much more likely to land:

- **Private and local-only.** No cloud, no telemetry, no socket — everything stays on your machine. The one exception is the model download for opt-in semantic search, which only pulls files down.
- **Small supply chain.** Eleven direct dependencies, one static binary. A new dependency needs a strong reason.
- **Claude Code only.** It archives Claude Code sessions; support for other tools (Gemini, OpenCode, …) is out of scope.
- **No server and no web dashboard.** The CLI and plugin keep everything local.

If your idea needs a server, a service to call, or cross-tool support, it is probably out of scope — but feel free to open it for discussion.
