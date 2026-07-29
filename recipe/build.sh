#!/usr/bin/env bash
# Build and install fresh-gui-backend + host UI assets (Linux package).
set -euo pipefail

: "${PREFIX:?PREFIX must be set}"
: "${SRC_DIR:?SRC_DIR must be set}"

cd "${SRC_DIR}"

# Pixi `global install --git` clones the repo without submodules. Local path
# builds may also lack vendor/fresh. Ensure the pinned Fresh tree is present.
ensure_vendor_fresh() {
  if [[ -f vendor/fresh/Cargo.toml ]]; then
    return 0
  fi

  if [[ -d .git || -f .git ]]; then
    echo "Initializing vendor/fresh submodule…"
    git submodule update --init --recursive -- vendor/fresh || true
    if [[ -f vendor/fresh/Cargo.toml ]]; then
      return 0
    fi
  fi

  local url rev pin_file
  pin_file="vendor/fresh.rev"
  url="https://github.com/amirhosseindavoody/fresh.git"
  if [[ -f .gitmodules ]]; then
    url="$(git config -f .gitmodules --get submodule.vendor/fresh.url || echo "${url}")"
  fi
  if [[ ! -f "${pin_file}" ]]; then
    echo "error: vendor/fresh is missing and ${pin_file} was not found." >&2
    echo "  For local clones: git submodule update --init --recursive" >&2
    exit 1
  fi
  rev="$(grep -E '^[0-9a-f]{7,40}$' "${pin_file}" | head -n1 || true)"
  if [[ -z "${rev}" ]]; then
    echo "error: ${pin_file} does not contain a git commit SHA" >&2
    exit 1
  fi
  echo "Fetching vendor/fresh @ ${rev} from ${url}…"
  rm -rf vendor/fresh
  mkdir -p vendor
  git init vendor/fresh
  git -C vendor/fresh remote add origin "${url}"
  git -C vendor/fresh fetch --depth 1 origin "${rev}"
  git -C vendor/fresh checkout --detach FETCH_HEAD
  test -f vendor/fresh/Cargo.toml
}

ensure_vendor_fresh

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
  --force \
  --no-track \
  --bin fresh-gui-backend \
  --root "${PREFIX}" \
  --path crates/fresh-gui-backend

echo "Installing UI assets…"
mkdir -p "${PREFIX}/share/fresh-gui/ui"
# Copy built assets but skip Vite sourcemaps (keeps the package lean).
if command -v rsync >/dev/null 2>&1; then
  rsync -a --exclude='*.map' crates/fresh-gui-app/ui/dist/ "${PREFIX}/share/fresh-gui/ui/"
else
  (cd crates/fresh-gui-app/ui/dist && tar -cf - --exclude='*.map' .) \
    | (cd "${PREFIX}/share/fresh-gui/ui" && tar -xf -)
fi
test -f "${PREFIX}/share/fresh-gui/ui/index.html"

echo "Bundling third-party licenses…"
cargo-bundle-licenses --format yaml --output ./THIRDPARTY.yml
