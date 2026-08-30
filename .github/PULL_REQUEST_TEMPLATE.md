## Why

State the problem and context.

## What changed?

1. Change
2. Change

## How to test?

1. Test step
2. Test step

## Pre-merge checklist

- [ ] `cargo test --locked` passes, including golden tests
- [ ] Run `scripts/bench.sh` for changes to `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema
- [ ] Run `cargo audit`; review the diff for security
- [ ] Update affected docs (`README.md`, `docs/*.md`, `CLAUDE.md`, skill docs)
- [ ] Make golden-file changes deliberate
