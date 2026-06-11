#!/bin/sh
# Find the subrosa binary and run the given hook event. If no binary exists yet,
# bootstrap the release pinned in hooks/binary-version for this platform —
# sha256-verified against hooks/sha256sums.txt committed in this repo — into the
# data dir. Everything is best-effort and quiet: a failed download must never
# break a Claude Code session.
EVENT="$1"
[ -n "$EVENT" ] || exit 0

DATA="${SUBROSA_DIR:-$HOME/.claude/subrosa}"
SELF="$(cd "$(dirname "$0")" && pwd)"

find_bin() {
  if command -v subrosa >/dev/null 2>&1; then
    echo subrosa
    return
  fi
  for c in "$HOME/.cargo/bin/subrosa" "$DATA/bin/subrosa" "$SELF/../bin/subrosa"; do
    if [ -x "$c" ]; then
      echo "$c"
      return
    fi
  done
}

bootstrap() {
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
  if ! curl -fsSL --proto '=https' --max-time 120 -o "$TMP/$ARCHIVE" \
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
  mkdir -p "$DATA/bin" && chmod 700 "$DATA" 2>/dev/null
  tar -xzf "$TMP/$ARCHIVE" -C "$DATA/bin" subrosa && chmod 755 "$DATA/bin/subrosa"
  STATUS=$?
  rm -rf "$TMP"
  return $STATUS
}

BIN="$(find_bin)"
if [ -z "$BIN" ]; then
  mkdir -p "$DATA" 2>/dev/null
  if bootstrap >>"$DATA/hook.log" 2>&1; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) bootstrap: installed $(cat "$SELF/binary-version") to $DATA/bin" >>"$DATA/hook.log"
  fi
  BIN="$(find_bin)"
  [ -n "$BIN" ] || exit 0
fi
exec "$BIN" hook "$EVENT"
