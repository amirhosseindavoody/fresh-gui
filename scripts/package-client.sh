#!/usr/bin/env bash
# Package the native GPUI host as a client archive.
#
# Usage:
#   scripts/package-client.sh <target> <version> [out-dir]
#
# Expects a release binary at target/<triple>/release/fresh-gui-app[.exe]
# (or target/release/ when that matches the host). The archive member is
# named fresh-gui[.exe] — that is the user-facing command. Cargo keeps the
# fresh-gui-app file name so it does not collide with the daemon binary.
#
# Writes under out-dir (default: dist/client):
#   fresh-gui-client-<version>-<target>.zip      (windows)
#   fresh-gui-client-<version>-<target>.tar.gz   (unix)
# and ${archive}.sha256 next to it.
#
# This archive is the host only. The Linux daemon is a separate release asset.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${1:?usage: package-client.sh <target> <version> [out-dir]}"
VERSION="${2:?usage: package-client.sh <target> <version> [out-dir]}"
OUT_DIR="${3:-$ROOT/dist/client}"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

cd "$ROOT"

case "$TARGET" in
  *windows* | *msvc* | *gnu*-pc-windows*)
    IS_WINDOWS=1
    CARGO_BIN="fresh-gui-app.exe"
    BIN_NAME="fresh-gui.exe"
    ARCHIVE_EXT="zip"
    ;;
  *)
    IS_WINDOWS=0
    CARGO_BIN="fresh-gui-app"
    BIN_NAME="fresh-gui"
    ARCHIVE_EXT="tar.gz"
    ;;
esac

find_binary() {
  local candidates=(
    "$ROOT/target/${TARGET}/release/${CARGO_BIN}"
    "$ROOT/target/release/${CARGO_BIN}"
    "$ROOT/target/${TARGET}/release/${BIN_NAME}"
    "$ROOT/target/release/${BIN_NAME}"
  )
  local base="${TARGET%%.*}"
  if [[ "$base" != "$TARGET" ]]; then
    candidates+=("$ROOT/target/${base}/release/${BIN_NAME}")
  fi
  local c
  for c in "${candidates[@]}"; do
    if [[ -f "$c" ]]; then
      echo "$c"
      return 0
    fi
  done
  echo "error: release client binary not found for target=${TARGET} (looked for ${CARGO_BIN})" >&2
  printf '  tried: %s\n' "${candidates[@]}" >&2
  exit 1
}

BINARY="$(find_binary)"
STAGE_NAME="fresh-gui-client-${VERSION}-${TARGET%%.*}"
ARCHIVE_STEM="$STAGE_NAME"
STAGE="$OUT_DIR/.stage/${STAGE_NAME}"
rm -rf "$STAGE"
mkdir -p "$STAGE"

cp -a "$BINARY" "$STAGE/${BIN_NAME}"
if [[ "$IS_WINDOWS" -eq 0 ]]; then
  chmod +x "$STAGE/${BIN_NAME}"
fi

cat >"$STAGE/README.txt" <<EOF
fresh-gui client ${VERSION}
target: ${TARGET%%.*}

This is the native GPUI host (${BIN_NAME}), not the Linux daemon.
Run it with no arguments on a machine that also has the daemon, or point it
at SSH. The repo installer names the headless binary fresh-gui-daemon when
both are installed. The daemon archive stays separate
(fresh-gui-*-x86_64-unknown-linux-gnu.tar.gz, member bin/fresh-gui).

  ./${BIN_NAME}
  ./${BIN_NAME} /path/to/project
  ./${BIN_NAME} user@server
  ./${BIN_NAME} remote add lab user@server --root /path/to/project
  ./${BIN_NAME} remote connect lab

If the remote has no fresh-gui binary, the host copies a Linux daemon to
~/.local/bin/fresh-gui, starts it headless, and tunnels ADE /ws to this window.

Optional daemon source (default: latest GitHub linux-gnu release):

  ./${BIN_NAME} remote daemon --path /path/to/fresh-gui
  ./${BIN_NAME} remote daemon --url https://github.com/amirhosseindavoody/fresh-gui/releases/download/v${VERSION}/fresh-gui-${VERSION}-x86_64-unknown-linux-gnu.tar.gz

Saved targets: %APPDATA%\\fresh-gui\\remotes.json on Windows,
~/.config/fresh-gui/remotes.json on Linux. Tokens are not stored there.

See https://github.com/amirhosseindavoody/fresh-gui
EOF

if [[ "$IS_WINDOWS" -eq 0 ]]; then
  cat >>"$STAGE/README.txt" <<'EOF'

Linux runtime: glibc >= 2.39 (Ubuntu 24.04 or newer), an X11 or Wayland
session, fontconfig, and a Vulkan loader (libvulkan.so.1, loaded on demand).
On Debian/Ubuntu:

  sudo apt install libvulkan1 libfontconfig1 libfreetype6 \
    libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libxcb1
EOF
fi

mkdir -p "$OUT_DIR"
ARCHIVE_PATH="$OUT_DIR/${ARCHIVE_STEM}.${ARCHIVE_EXT}"
rm -f "$ARCHIVE_PATH"

(
  cd "$OUT_DIR/.stage"
  if [[ "$IS_WINDOWS" -eq 1 ]]; then
    if command -v zip >/dev/null 2>&1; then
      zip -r -q "$ARCHIVE_PATH" "$STAGE_NAME"
    else
      tar -a -cf "$ARCHIVE_PATH" "$STAGE_NAME"
    fi
  else
    tar -czf "$ARCHIVE_PATH" "$STAGE_NAME"
  fi
)

rm -rf "$OUT_DIR/.stage"

SUM_FILE="${ARCHIVE_PATH}.sha256"
(
  cd "$OUT_DIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$(basename "$ARCHIVE_PATH")"
  else
    shasum -a 256 "$(basename "$ARCHIVE_PATH")"
  fi
) | tee "$SUM_FILE"

echo "Wrote ${ARCHIVE_PATH}"
echo "Wrote ${SUM_FILE}"
