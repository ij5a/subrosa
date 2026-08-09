#!/bin/sh
# Proof that the background indexer outlives the session that started it.
# A wrapper runs the SessionStart hook and exits; the whole wrapper process
# group is then killed, the way a shell kills a job. PASS = the indexer is
# still running and has reparented to init (ppid 1). If this ever fails, a
# first index dies with the session that started it and never finishes.
# Needs job control (`set -m`) so the wrapper gets a process group of its own.
# Usage: scripts/detach-test.sh [path-to-subrosa-binary]
set -eu

BIN="${1:-target/release/subrosa}"
[ -x "$BIN" ] || { echo "detach-test: binary not found at $BIN (cargo build first)"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

ROOT="$(mktemp -d)"
fail() { echo "DETACH FAIL: $1" >&2; exit 1; }
cleanup() {
  # The indexer's own process group, so its curl goes with it.
  [ -n "${WORKER:-}" ] && kill -TERM "-$WORKER" 2>/dev/null || true
  rm -rf "$ROOT"
}
trap cleanup EXIT

export SUBROSA_DIR="$ROOT/data"
export SUBROSA_PROJECTS_DIR="$ROOT/projects"
unset SUBROSA_MIRROR SUBROSA_MIRROR_PASSPHRASE SUBROSA_DB SUBROSA_SEMANTIC 2>/dev/null || true
mkdir -p "$SUBROSA_PROJECTS_DIR/-tmp-demo" "$ROOT/bin"

# A curl that never finishes, so the indexer stays alive at the download while
# this script does its checks — and no real bytes are ever fetched. Named
# through SUBROSA_CURL, not PATH: the fetcher resolves curl by absolute path on
# purpose, so a test must not lean on the lookup that was removed.
#
# It also records its own parent, which IS the indexer this test started. That
# is the only way to be sure: `pgrep` would happily hand back somebody else's
# worker from the same dev binary, which would pass the test without ever
# testing our spawn, and then get SIGTERMed on the way out.
printf '#!/bin/sh\necho "$PPID" > "%s"\nsleep 60\n' "$ROOT/worker.pid" > "$ROOT/bin/curl"
chmod +x "$ROOT/bin/curl"
export SUBROSA_CURL="$ROOT/bin/curl"

# One turn is enough work for the indexer to decide it has something to do.
T="$SUBROSA_PROJECTS_DIR/-tmp-demo/detach.jsonl"
printf '{"type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u0","cwd":"/tmp/demo","message":{"role":"user","content":"detach test turn"}}\n' > "$T"
"$BIN" init >/dev/null
"$BIN" ingest "$T" >/dev/null

cat > "$ROOT/wrapper.sh" <<EOF
#!/bin/sh
ps -o pgid= -p \$\$ | tr -d ' ' > "$ROOT/wrapper.pgid"
printf '{"session_id":"detach","cwd":"/tmp/demo"}' | "$BIN" hook session-start >/dev/null 2>&1
EOF
chmod +x "$ROOT/wrapper.sh"

# Job control on: a background job becomes its own process group leader, which
# is what makes `kill -TERM -PGID` mean "the session", not "this script too".
set -m
"$ROOT/wrapper.sh" &
W=$!
wait "$W" || true
set +m

PGID="$(cat "$ROOT/wrapper.pgid" 2>/dev/null || echo '')"
[ -n "$PGID" ] || fail "the wrapper never recorded its process group"
[ "$PGID" = "$W" ] || fail "the wrapper is not its own group leader (pgid $PGID, pid $W) — job control off?"

# The hook returns before the indexer has reached the download, so give the
# spawn a moment. The pid comes from our own fake curl, so it is this test's
# worker and nobody else's.
WORKER=""
i=0
while [ "$i" -lt 100 ]; do
  WORKER="$(cat "$ROOT/worker.pid" 2>/dev/null || true)"
  [ -n "$WORKER" ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -n "$WORKER" ] || fail "the hook did not start the indexer"
ps -p "$WORKER" -o args= | grep -q "embed --auto" || fail "pid $WORKER is not our indexer: $(ps -p "$WORKER" -o args=)"
echo "detach: indexer pid $WORKER, wrapper pgid $PGID (wrapper has exited)"

kill -TERM "-$PGID" 2>/dev/null || true
sleep 1

kill -0 "$WORKER" 2>/dev/null || fail "the indexer died with the session's process group"
PPID_NOW="$(ps -o ppid= -p "$WORKER" | tr -d ' ')"
[ "$PPID_NOW" = "1" ] || fail "the indexer did not reparent to init (ppid $PPID_NOW)"
WPGID="$(ps -o pgid= -p "$WORKER" | tr -d ' ')"
[ "$WPGID" = "$WORKER" ] || fail "the indexer is not in a process group of its own (pgid $WPGID)"

echo "detach: survived kill -TERM -$PGID, ppid now 1, own process group $WPGID"
echo "detach: PASS"
