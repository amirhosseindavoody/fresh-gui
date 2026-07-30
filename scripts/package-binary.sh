#!/usr/bin/env bash
# Package a standalone fresh-gui binary archive (bin + embedded UI assets).
#
# Usage:
#   scripts/package-binary.sh <target> <version> [out-dir]
#
# Expects:
#   - release binary at target/<triple>/release/fresh-gui[.exe]
#     (or target/release/ when target matches host and no triple dir)
#   - UI assets at crates/fresh-gui-app/ui/dist/index.html
#
# Writes under out-dir (default: dist/binaries):
#   fresh-gui-<version>-<target>.tar.gz   (unix)
#   fresh-gui-<version>-<target>.zip      (windows)
# and writes ${archive}.sha256 next to the archive

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${1:?usage: package-binary.sh <target> <version> [out-dir]}"
VERSION="${2:?usage: package-binary.sh <target> <version> [out-dir]}"
OUT_DIR="${3:-$ROOT/dist/binaries}"
# Resolve out-dir up front so relative paths stay valid after we cd into .stage.
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

cd "$ROOT"

case "$TARGET" in
  *windows* | *msvc* | *gnu*-pc-windows*)
    IS_WINDOWS=1
    BIN_NAME="fresh-gui.exe"
    ARCHIVE_EXT="zip"
    ;;
  *)
    IS_WINDOWS=0
    BIN_NAME="fresh-gui"
    ARCHIVE_EXT="tar.gz"
    ;;
esac

find_binary() {
  local candidates=(
    "$ROOT/target/${TARGET}/release/${BIN_NAME}"
    "$ROOT/target/release/${BIN_NAME}"
  )
  # cargo-zigbuild with glibc suffix (e.g. x86_64-unknown-linux-gnu.2.31)
  # still writes under the base triple directory.
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
  echo "error: release binary not found for target=${TARGET} (looked for ${BIN_NAME})" >&2
  printf '  tried: %s\n' "${candidates[@]}" >&2
  exit 1
}

BINARY="$(find_binary)"
UI_DIST="$ROOT/crates/fresh-gui-app/ui/dist"
if [[ ! -f "$UI_DIST/index.html" ]]; then
  echo "error: UI not built (missing ${UI_DIST}/index.html). Run: pixi run ui-build" >&2
  exit 1
fi

STAGE_NAME="fresh-gui-${VERSION}-${TARGET%%.*}"
# Prefer the full target string (without glibc suffix) in the archive stem.
ARCHIVE_STEM="fresh-gui-${VERSION}-${TARGET%%.*}"
STAGE="$OUT_DIR/.stage/${STAGE_NAME}"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/fresh-gui/ui"

cp -a "$BINARY" "$STAGE/bin/${BIN_NAME}"
if [[ "$IS_WINDOWS" -eq 0 ]]; then
  chmod +x "$STAGE/bin/${BIN_NAME}"
fi

if command -v rsync >/dev/null 2>&1; then
  rsync -a --exclude='*.map' "$UI_DIST"/ "$STAGE/share/fresh-gui/ui/"
else
  (cd "$UI_DIST" && tar -cf - --exclude='*.map' .) \
    | (cd "$STAGE/share/fresh-gui/ui" && tar -xf -)
fi
test -f "$STAGE/share/fresh-gui/ui/index.html"

cat >"$STAGE/README.txt" <<EOF
fresh-gui ${VERSION}
target: ${TARGET%%.*}

Layout (same as the pixi/conda package):
  bin/${BIN_NAME}
  share/fresh-gui/ui/   # embedded browser UI

Run from this directory (or put bin/ on PATH and keep the relative
share/ layout next to bin/):

  ./bin/${BIN_NAME}
  ./bin/${BIN_NAME} --foreground

Stop a background session with:

  ./bin/${BIN_NAME} close

See https://github.com/amirhosseindavoody/fresh-gui for docs.
EOF

mkdir -p "$OUT_DIR"
ARCHIVE_PATH="$OUT_DIR/${ARCHIVE_STEM}.${ARCHIVE_EXT}"
rm -f "$ARCHIVE_PATH"

(
  cd "$OUT_DIR/.stage"
  if [[ "$IS_WINDOWS" -eq 1 ]]; then
    # Prefer Info-ZIP when present; otherwise bsdtar on Windows (GH Actions) can
    # emit .zip via auto-compress from the extension.
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
