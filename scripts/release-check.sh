#!/bin/sh
# The hard release gate, in one run: regression, performance, token usage, and
# smoke. Any non-zero aborts. Run this GREEN on the exact commit you are about
# to tag — not on an earlier state and not "it was fine during dev".
# Security review (/security-review) and docs-in-sync are the two gates a script
# can't run; do those by hand per CLAUDE.md before tagging.
# Usage: scripts/release-check.sh
set -eu
cd "$(git rev-parse --show-toplevel)"

echo "==> [1/4] regression — cargo test --locked (unit + golden)"
cargo test --locked

echo "==> building release binary for the perf + smoke gates"
cargo build --release --locked
BIN="target/release/subrosa"

echo "==> [2/4 + 3/4] performance + token usage — scripts/bench.sh"
# bench.sh times the recall/search/ingest hot paths AND measures the recall
# injection's token cost, exiting non-zero if it blows past the ~180-token
# promise (the 220-token guard). Both gates, one script.
scripts/bench.sh "$BIN"

echo "==> [4/4] smoke — scripts/smoke.sh (end-to-end on the built binary)"
scripts/smoke.sh "$BIN"

echo "==> release-check: ALL GREEN"
