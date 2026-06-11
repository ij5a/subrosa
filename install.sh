#!/bin/sh
# Install a prebuilt subrosa binary from GitHub Releases. No Rust needed.
#   curl -fsSL https://raw.githubusercontent.com/ij5a/subrosa/main/install.sh | sh
# Optional: VERSION=v0.1.0 sh install.sh   (defaults to the latest release)
set -eu

REPO="ij5a/subrosa"
DEST="${SUBROSA_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)              TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64)             TARGET=x86_64-apple-darwin ;;
  Linux-x86_64)              TARGET=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) echo "unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

if [ -z "${VERSION:-}" ]; then
  # The /releases/latest redirect ends in the tag name.
  VERSION="$(curl -fsSI "https://github.com/$REPO/releases/latest" \
    | tr -d '\r' | awk -F'/tag/' 'tolower($0) ~ /^location:/ {print $2}')"
fi
[ -n "$VERSION" ] || { echo "could not resolve the latest release tag" >&2; exit 1; }

ARCHIVE="subrosa-$VERSION-$TARGET.tar.gz"
BASE="https://github.com/$REPO/releases/download/$VERSION"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading $ARCHIVE ..."
curl -fsSL --proto '=https' -o "$TMP/$ARCHIVE" "$BASE/$ARCHIVE"
curl -fsSL --proto '=https' -o "$TMP/sha256sums.txt" "$BASE/sha256sums.txt"

WANT="$(awk -v f="$ARCHIVE" '$2 == f {print $1}' "$TMP/sha256sums.txt")"
if command -v sha256sum >/dev/null 2>&1; then
  GOT="$(sha256sum "$TMP/$ARCHIVE" | awk '{print $1}')"
else
  GOT="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{print $1}')"
fi
[ -n "$WANT" ] && [ "$GOT" = "$WANT" ] || { echo "checksum mismatch — aborting" >&2; exit 1; }

mkdir -p "$DEST"
tar -xzf "$TMP/$ARCHIVE" -C "$DEST" subrosa
chmod 755 "$DEST/subrosa"
echo "installed $DEST/subrosa ($VERSION)"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "note: $DEST is not on your PATH — add it to your shell profile" ;;
esac
