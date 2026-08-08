#!/bin/sh
# End-to-end smoke of a BUILT subrosa binary against a throwaway archive
# (never live data). Drives the real ingest pipeline, then asserts redaction,
# the encrypted-mirror + restore paths, the per-project budget override, and
# the hooks-always-exit-0 invariant. Exits 1 on the first failed check.
# Usage: scripts/smoke.sh [path-to-subrosa-binary]
set -eu

BIN="${1:-target/release/subrosa}"
[ -x "$BIN" ] || { echo "smoke: binary not found at $BIN (cargo build --release first)"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
export SUBROSA_DIR="$ROOT/data"
export SUBROSA_PROJECTS_DIR="$ROOT/projects"
# Start from a clean mirror config so a developer's own env can't skew the run.
unset SUBROSA_MIRROR SUBROSA_MIRROR_PASSPHRASE SUBROSA_DB 2>/dev/null || true
mkdir -p "$SUBROSA_PROJECTS_DIR/-tmp-demo"

fail() { echo "SMOKE FAIL: $1" >&2; exit 1; }

# One user transcript record. Args: text, index.
rec() {
  printf '{"type":"user","timestamp":"2026-01-01T00:00:0%sZ","uuid":"u%s","cwd":"/tmp/demo","message":{"role":"user","content":"%s"}}\n' "$2" "$2" "$1"
}

# --- pipeline + redaction ---
# xyzzycontrol is a non-secret control that must survive. The password and the
# multi-word passphrase are secret-shaped and must be masked before storage.
T="$SUBROSA_PROJECTS_DIR/-tmp-demo/smoke.jsonl"
{
  rec "healthcheck endpoint xyzzycontrol is responding" 0
  rec "login with password=notarealpw123 then continue" 1
  rec "mirror_passphrase=zzalpha zzbravo zzcharlie zzdelta" 2
} > "$T"

"$BIN" init >/dev/null
"$BIN" ingest "$T" >/dev/null

DUMP="$("$BIN" session smoke)"
[ -n "$DUMP" ] || fail "session dump empty — ingest or session lookup broke"
echo "$DUMP" | grep -q xyzzycontrol || fail "control word missing — turn was not ingested"
for w in notarealpw123 zzalpha zzbravo zzcharlie zzdelta; do
  if echo "$DUMP" | grep -q "$w"; then fail "secret '$w' reached the archive — redaction broke"; fi
done
echo "$DUMP" | grep -q 'redacted' || fail "no redaction marker in the stored turn"
echo "smoke: pipeline + redaction ok"

# --- encrypted mirror + restore ---
MIRROR="$ROOT/mirror"
mkdir -p "$MIRROR"
export SUBROSA_MIRROR="$MIRROR" SUBROSA_MIRROR_PASSPHRASE="smoke-test-phrase"
"$BIN" backup --force >/dev/null
[ -f "$MIRROR/subrosa-latest.db.enc" ] || fail "encrypted mirror not written"
if [ -f "$MIRROR/subrosa-latest.db" ]; then fail "plaintext mirror present next to the .enc"; fi
head -c 8 "$MIRROR/subrosa-latest.db.enc" | grep -q SUBROSA1 || fail "mirror missing the SUBROSA1 header"

OUT="$ROOT/restored.db"
"$BIN" restore "$MIRROR/subrosa-latest.db.enc" --out "$OUT" >/dev/null
head -c 15 "$OUT" | grep -q "SQLite format 3" || fail "restore did not produce a SQLite database"
if "$BIN" restore "$MIRROR/subrosa-latest.db.enc" --out "$OUT" >/dev/null 2>&1; then fail "restore overwrote an existing file"; fi
if SUBROSA_MIRROR_PASSPHRASE=wrong "$BIN" restore "$MIRROR/subrosa-latest.db.enc" --out "$ROOT/w.db" >/dev/null 2>&1; then fail "restore accepted a wrong passphrase"; fi
if [ -f "$ROOT/w.db" ]; then fail "restore wrote output on a wrong passphrase"; fi
unset SUBROSA_MIRROR SUBROSA_MIRROR_PASSPHRASE
echo "smoke: encrypted mirror + restore ok"

# --- per-project budget override, fail-closed on a bad file ---
MD="$ROOT/notes"
mkdir -p "$MD"
{ echo '---'; echo 'name: n'; echo 'description: d'; echo 'metadata:'; echo '  type: reference'; echo '---'; echo 'body text'; } > "$MD/n.md"
"$BIN" fact upsert --memdir "$MD" --leaf n.md --hook "smoke fact" >/dev/null

# --- fact doctor: a well-formed, registered leaf lints clean at exit 0 ---
DOC="$("$BIN" fact doctor --memdir "$MD")" || fail "doctor exited non-zero on a clean memdir"
echo "$DOC" | grep -q 'doctor: 1 leaf(s), 1 fact(s) — clean' || fail "doctor did not report a clean memdir: $DOC"
echo "smoke: fact doctor ok"

printf '30000\n' > "$MD/.budget"
"$BIN" generate --memdir "$MD" >/dev/null || fail "a good .budget was rejected"
SUM1="$(cksum "$MD/MEMORY.md")"
rm -f "$MD/.budget"
mkdir "$MD/.budget"  # a directory is unreadable as a budget file
if "$BIN" generate --memdir "$MD" >/dev/null 2>&1; then fail "an unreadable .budget did not fail generate"; fi
SUM2="$(cksum "$MD/MEMORY.md")"
[ "$SUM1" = "$SUM2" ] || fail "MEMORY.md was rewritten despite an unreadable .budget"
rmdir "$MD/.budget"
echo "smoke: budget override ok"

# --- hooks always exit 0 ---
printf '{"prompt":"ping xyzzycontrol","cwd":"/tmp/demo","session_id":"smoke"}\n' | "$BIN" hook user-prompt-submit >/dev/null || fail "user-prompt-submit hook exited non-zero"
printf '{"transcript_path":"%s","session_id":"smoke"}\n' "$T" | "$BIN" hook stop >/dev/null || fail "stop hook exited non-zero"
printf '{"transcript_path":"%s","session_id":"smoke"}\n' "$T" | "$BIN" hook session-end >/dev/null || fail "session-end hook exited non-zero"
echo "smoke: hooks exit-0 ok"

echo "smoke: all checks passed"
