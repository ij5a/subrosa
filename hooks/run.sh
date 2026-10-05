#!/bin/sh
# Find the subrosa binary and run the given hook event. If no binary exists yet,
# or at session start the data-dir binary does not match hooks/binary-version,
# download that release for this platform, sha256-verified against
# hooks/sha256sums.txt committed in this repo, into the data dir. Everything is
# best-effort and quiet: a failed download must never break a Claude Code session.
EVENT="$1"
[ -n "$EVENT" ] || exit 0

DATA="${SUBROSA_DIR:-$HOME/.claude/subrosa}"
# Parameter expansion, not $(dirname): the hot path must not fork. hooks.json
# always invokes us by absolute path, so stripping the last component works.
SELF="${0%/*}"
[ "$SELF" != "$0" ] || SELF="."

# Sets BIN directly (no $(...) capture subshell — this runs on every prompt).
find_bin() {
  if command -v subrosa >/dev/null 2>&1; then
    BIN=subrosa
    return 0
  fi
  for c in "$HOME/.cargo/bin/subrosa" "$DATA/bin/subrosa" "$SELF/../bin/subrosa"; do
    if [ -x "$c" ]; then
      BIN="$c"
      return 0
    fi
  done
  BIN=""
}

bootstrap() {
  # Owner-only for everything the bootstrap creates (data dir, temp download).
  umask 077
  [ -s "$SELF/binary-version" ] && [ -s "$SELF/sha256sums.txt" ] || return 1
  VERSION="$(cat "$SELF/binary-version")"
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)              TARGET=aarch64-apple-darwin ;;
    Darwin-x86_64)             TARGET=x86_64-apple-darwin ;;
    Linux-x86_64)              TARGET=x86_64-unknown-linux-musl ;;
    Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
    *) return 1 ;;
  esac
  ARCHIVE="subrosa-$VERSION-$TARGET.tar.gz"
  WANT="$(awk -v f="$ARCHIVE" '$2 == f {print $1}' "$SELF/sha256sums.txt")"
  [ -n "$WANT" ] || return 1

  TMP="$(mktemp -d)" || return 1
  if ! curl -fsSL --proto '=https' --proto-redir '=https' --max-time 120 -o "$TMP/$ARCHIVE" \
    "https://github.com/ij5a/subrosa/releases/download/$VERSION/$ARCHIVE"; then
    rm -rf "$TMP"
    return 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    GOT="$(sha256sum "$TMP/$ARCHIVE" | awk '{print $1}')"
  else
    GOT="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{print $1}')"
  fi
  if [ "$GOT" != "$WANT" ]; then
    rm -rf "$TMP"
    return 1
  fi
  mkdir -p "$DATA/bin" && chmod 700 "$DATA" "$DATA/bin" 2>/dev/null
  # Write beside the target, then rename, so a running old binary is never rewritten in place.
  NEW="$DATA/bin/subrosa.$$"
  tar -xzf "$TMP/$ARCHIVE" -O subrosa >"$NEW" && chmod 755 "$NEW" && mv -f "$NEW" "$DATA/bin/subrosa"
  STATUS=$?
  rm -rf "$TMP" "$NEW"
  return $STATUS
}

find_bin

# Session start only: a failed download then retries once per session, not on every prompt.
# distill.rs puts this copy first on its child's PATH, so refresh it even when PATH wins.
if [ "$EVENT" = session-start ] && [ -x "$DATA/bin/subrosa" ]; then
  PIN="$(cat "$SELF/binary-version" 2>/dev/null)"
  if [ "$("$DATA/bin/subrosa" -V 2>/dev/null)" != "subrosa ${PIN#v}" ] && bootstrap >>"$DATA/hook.log" 2>&1; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) bootstrap: installed $PIN to $DATA/bin" >>"$DATA/hook.log"
  fi
fi
if [ -z "$BIN" ]; then
  mkdir -p "$DATA" 2>/dev/null
  if bootstrap >>"$DATA/hook.log" 2>&1; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) bootstrap: installed $(cat "$SELF/binary-version") to $DATA/bin" >>"$DATA/hook.log"
  fi
  find_bin
  [ -n "$BIN" ] || exit 0
fi
# No exec, and stderr dropped: an older PATH binary that doesn't know this
# event yet must degrade to a silent no-op, never a failed hook.
"$BIN" hook "$EVENT" 2>/dev/null
exit 0
