## Why

The problem and the context, in a short paragraph.

## What changed?

1.
2.

## How to test?

1.
2.

## Pre-merge checklist

- [ ] `cargo test --locked` passes (including the golden tests)
- [ ] Ran `scripts/bench.sh` if this touches `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema
- [ ] Ran `cargo audit` and reviewed the diff for security
- [ ] Updated any affected docs (`README.md`, `docs/*.md`, `CLAUDE.md`, skill docs)
- [ ] Any golden-file change is deliberate, not just silencing a failing test
