#!/bin/sh
# Reproducible performance benchmark against a synthetic archive (never live data).
# Needs: hyperfine. Usage: scripts/bench.sh [path-to-subrosa-binary]
set -eu

BIN="${1:-target/release/subrosa}"
BENCH="${BENCH_DIR:-/tmp/subrosa-bench}"
SESSIONS="${BENCH_SESSIONS:-200}"
TURNS="${BENCH_TURNS:-250}"

command -v hyperfine >/dev/null 2>&1 || { echo "bench: hyperfine not installed"; exit 1; }
[ -x "$BIN" ] || { echo "bench: binary not found at $BIN (cargo build --release first)"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

export SUBROSA_DIR="$BENCH/data"
export SUBROSA_PROJECTS_DIR="$BENCH/projects"
# No background indexer during a benchmark: it would download the model and
# compete for cores with the very thing being timed.
export SUBROSA_SEMANTIC=off

# Deterministic synthetic transcripts: prose + identifiers + tool records across
# 8 projects, shaped like real Claude Code JSONL. awk with a hand-rolled LCG so
# the corpus is byte-identical on every awk (no srand portability gamble).
if [ ! -f "$BENCH/projects/.generated" ]; then
  rm -rf "$BENCH"
  mkdir -p "$BENCH/projects"
  awk -v root="$BENCH/projects" -v sessions="$SESSIONS" -v turns="$TURNS" 'BEGIN {
    seed = 42
    nw = split("deploy rollout cluster ingress gateway terraform module bucket lambda queue " \
               "retry timeout migration schema index vacuum replica failover snapshot backup " \
               "latency throughput budget alert dashboard pipeline runner artifact release " \
               "candidate incident postmortem rollback canary traffic shard partition broker", words, " ")
    ni = split("svc-cache-prod svc-auth-prod svc-billing-prod svc-search-prod svc-ingest-prod " \
               "cache-gateway-prod us-east-1", idents, " ")
    for (n = 1000; n < 1040; n++) idents[++ni] = "TICKET-" n
    for (s = 0; s < sessions; s++) {
      proj = "-tmp-bench-proj" (s % 8)
      cwd = "/tmp/bench/proj" (s % 8)
      system("mkdir -p \"" root "/" proj "\"")
      f = sprintf("%s/%s/bench-%04d-0000-4000-8000-%012d.jsonl", root, proj, s, s)
      for (t = 0; t < turns; t++) {
        ts = sprintf("2026-01-%02dT%02d:%02d:00Z", (s % 28) + 1, int(t / 60) % 24, t % 60)
        if (t % 2 == 0) {
          printf "{\"type\":\"user\",\"timestamp\":\"%s\",\"uuid\":\"u%d-%d\",\"cwd\":\"%s\"," \
                 "\"message\":{\"role\":\"user\",\"content\":\"%s\"}}\n", ts, s, t, cwd, sentence() > f
        } else {
          blocks = "{\"type\":\"text\",\"text\":\"" sentence() " " sentence() "\"}"
          if (t % 5 == 1)
            blocks = blocks ",{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":" \
                     "{\"command\":\"kubectl get pods -n " pick(idents, ni) "\"}}"
          printf "{\"type\":\"assistant\",\"timestamp\":\"%s\",\"uuid\":\"a%d-%d\",\"cwd\":\"%s\"," \
                 "\"message\":{\"role\":\"assistant\",\"content\":[%s]}}\n", ts, s, t, cwd, blocks > f
        }
      }
      close(f)
    }
    printf "generated %d transcripts x %d records under %s\n", sessions, turns, root
  }
  # Park-Miller minstd: products stay under 2^53, exact in awk doubles.
  function rnd(n) { seed = (seed * 16807) % 2147483647; return seed % n }
  function pick(arr, n) { return arr[rnd(n) + 1] }
  function sentence(   k, i, out) {
    k = 8 + rnd(13)
    out = ""
    for (i = 0; i < k; i++) out = out (i ? " " : "") words[rnd(nw) + 1]
    if (rnd(10) < 4) out = out " " pick(idents, ni)
    return out
  }'
  touch "$BENCH/projects/.generated"
fi

# Fresh full ingest: archive build throughput (3 runs, DB wiped in prepare).
echo "== ingest: full archive build ($SESSIONS transcripts x $TURNS records) =="
hyperfine --warmup 1 --runs 3 \
  --prepare "rm -rf '$SUBROSA_DIR'" \
  "'$BIN' ingest --sweep --quiet"

[ -f "$SUBROSA_DIR/memory.db" ] || "$BIN" ingest --sweep --quiet
TURNS_TOTAL=$("$BIN" init | sed -n 's/.*turns=//p')
DB_KB=$(du -k "$SUBROSA_DIR/memory.db" | cut -f1)
echo "archive: $TURNS_TOTAL turns, $((DB_KB / 1024))MB"
echo

HIT='{"prompt":"how did the cache-gateway-prod TICKET-1012 deploy rollout go","cwd":"/tmp/bench/proj3","session_id":"bench-live"}'
MISS='{"prompt":"zxqv-flurble-9921 quuxotic zebra contraption rebalance","cwd":"/tmp/bench/proj3","session_id":"bench-live"}'

echo "== hook user-prompt-submit (recall): every-prompt hot path =="
hyperfine --warmup 5 --runs 50 \
  --prepare "rm -f '$SUBROSA_DIR/recall-seen.log'" \
  -n "recall (match + inject)" "printf '%s' '$HIT' | '$BIN' hook user-prompt-submit" \
  -n "recall (no match, silent)" "printf '%s' '$MISS' | '$BIN' hook user-prompt-submit"

# Recall injection size: the per-prompt token cost behind the "~180 tokens" promise.
# hyperfine (above) times the hook; this weighs what it actually emits into context.
echo "== recall injection: token cost of a strong match =="
rm -f "$SUBROSA_DIR/recall-seen.log"   # the timed runs above logged this session; clear it or dedup hides the hit
INJECT="$(printf '%s' "$HIT" | "$BIN" hook user-prompt-submit)"
IBYTES=$(printf '%s' "$INJECT" | wc -c | tr -d ' ')
ISNIPS=$(printf '%s\n' "$INJECT" | grep -c '^- ' || true)
# Estimate only: bytes/3.8 is the docs' heuristic (23 KB index ~= 6k tokens), not a real tokenizer.
ITOK=$(awk -v b="$IBYTES" 'BEGIN { printf "%.0f", b / 3.8 }')
echo "injected $ISNIPS snippet(s), $IBYTES bytes ~= $ITOK tokens (est., bytes/3.8 — not a tokenizer)"
printf '%s\n' "$INJECT" | sed 's/^/    | /'
# Gross-regression guard: the MAX_INJECT x SNIPPET_CHARS cap should hold recall near ~180 tokens.
# This trips only if the cap logic breaks (e.g. SNIPPET_CHARS bumped); it sits above the heavy-match
# worst case (~199 tok with full match-marked snippets, measured 2026-06-16), so real hits never trip it.
CEILING_TOKENS=220
if [ "$ITOK" -gt "$CEILING_TOKENS" ]; then
  echo "bench: recall injection ${ITOK} tokens exceeds the ${CEILING_TOKENS}-token guard — recall cap regressed?" >&2
  exit 1
fi
echo

echo "== hook session-start (idle catch-up sweep over $SESSIONS transcripts) =="
hyperfine --warmup 3 --runs 25 \
  -n "session-start (nothing changed)" \
  "printf '{\"cwd\":\"/tmp/bench/proj3\",\"session_id\":\"bench-live\"}' | '$BIN' hook session-start"

# Per-turn live-ingest: resumes from the stored byte cursor and reads only the new
# bytes of the one active transcript (single file, not the full sweep). Re-ingesting
# an already-archived 250-record file is the steady-state per-turn cost — now flat
# regardless of how long the session has grown.
echo "== hook stop (per-turn incremental ingest of the in-progress transcript) =="
STOP_SID="bench-0003-0000-4000-8000-000000000003"
STOP_TP="$SUBROSA_PROJECTS_DIR/-tmp-bench-proj3/$STOP_SID.jsonl"
hyperfine --warmup 5 --runs 50 \
  -n "stop (re-read a 250-record live transcript)" \
  "printf '{\"transcript_path\":\"$STOP_TP\",\"session_id\":\"$STOP_SID\",\"cwd\":\"/tmp/bench/proj3\"}' | '$BIN' hook stop"

echo "== search =="
hyperfine --warmup 3 --runs 25 \
  -n "search identifier" "'$BIN' search cache-gateway-prod -n 5" \
  -n "search two terms" "'$BIN' search deploy rollout -n 5"

# Fuzzy: a substring hit, then the nearest-match fallback (zero substring rows →
# trigram-OR candidates + one-edit filter) — the fallback rescue is the worst case.
echo "== search --fuzzy (substring hit, typo fallback, true miss) =="
"$BIN" search --fuzzy latency -n 1 >/dev/null 2>&1 || true  # one-time trigram index build, outside timing
hyperfine --warmup 3 --runs 25 \
  -n "fuzzy substring hit" "'$BIN' search --fuzzy atency -n 5" \
  -n "fuzzy typo fallback (hit)" "'$BIN' search --fuzzy latecny -n 5" \
  -n "fuzzy true miss (fallback empty)" "'$BIN' search --fuzzy qqqqzzzz -n 5"

# Co-occurrence verb: a focused identifier (few sessions) vs a ubiquitous word
# (every session — the worst case that the session-scan cap bounds).
echo "== related (co-occurrence over the archive) =="
hyperfine --warmup 2 --runs 15 \
  -n "related identifier" "'$BIN' related svc-cache-prod -n 10" \
  -n "related common word (worst case)" "'$BIN' related deploy -n 10"

echo "== process startup floor =="
hyperfine --warmup 5 --runs 50 -n "subrosa --version" "'$BIN' --version"

# Wrapper overhead: what Claude Code actually pays per hook fire (script + binary).
RUNSH="$(cd "$(dirname "$0")/.." && pwd)/hooks/run.sh"
echo "== hooks/run.sh wrapper (PATH resolves to the bench binary) =="
PATH="$(dirname "$BIN"):$PATH" hyperfine --warmup 5 --runs 50 \
  --prepare "rm -f '$SUBROSA_DIR/recall-seen.log'" \
  -n "run.sh user-prompt-submit" "printf '%s' '$MISS' | sh '$RUNSH' user-prompt-submit"
