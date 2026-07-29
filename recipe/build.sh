#!/usr/bin/env bash
# Build and install fresh-gui-backend + host UI assets (Linux package).
set -euo pipefail

: "${PREFIX:?PREFIX must be set}"
: "${SRC_DIR:?SRC_DIR must be set}"

cd "${SRC_DIR}"

if [[ ! -f vendor/fresh/Cargo.toml ]]; then
  echo "error: vendor/fresh submodule is missing; run:" >&2
  echo "  git submodule update --init --recursive" >&2
  exit 1
fi

echo "Building host UI…"
(
  cd crates/fresh-gui-app/ui
  bun install --frozen-lockfile
  bun run build
  test -f dist/index.html
)

echo "Installing fresh-gui-backend…"
export CARGO_PROFILE_RELEASE_STRIP="${CARGO_PROFILE_RELEASE_STRIP:-symbols}"
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-fat}"
cargo auditable install \
  --locked \
  --no-track \
  --bin fresh-gui-backend \
  --root "${PREFIX}" \
  --path crates/fresh-gui-backend

echo "Installing UI assets…"
mkdir -p "${PREFIX}/share/fresh-gui/ui"
cp -a crates/fresh-gui-app/ui/dist/. "${PREFIX}/share/fresh-gui/ui/"
test -f "${PREFIX}/share/fresh-gui/ui/index.html"

echo "Bundling third-party licenses…"
cargo-bundle-licenses --format yaml --output ./THIRDPARTY.yml
