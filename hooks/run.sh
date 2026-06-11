#!/bin/sh
# Find the subrosa binary and run the given hook event. A missing binary is a
# quiet no-op — a memory problem must never break a Claude Code session.
EVENT="$1"
[ -n "$EVENT" ] || exit 0

if command -v subrosa >/dev/null 2>&1; then
  BIN=subrosa
elif [ -x "$HOME/.cargo/bin/subrosa" ]; then
  BIN="$HOME/.cargo/bin/subrosa"
elif [ -n "$CLAUDE_PLUGIN_ROOT" ] && [ -x "$CLAUDE_PLUGIN_ROOT/bin/subrosa" ]; then
  BIN="$CLAUDE_PLUGIN_ROOT/bin/subrosa"
else
  exit 0
fi

exec "$BIN" hook "$EVENT"
