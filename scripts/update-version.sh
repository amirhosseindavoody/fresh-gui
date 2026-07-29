#!/usr/bin/env bash
# Bump project version in Cargo.toml, pixi.toml, and recipe/recipe.yaml (if present).
#
# Scheme (Cargo-compatible SemVer + date sense):
#   YYYY.MMDD.N   e.g. 2026.630.1  (2026-06-30, first release that day)
#
# MMDD is month*100+day (no leading zeros; unique per calendar day).
# N starts at 1 on the first bump of a calendar day; further runs the same day increment N.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_TOML="$ROOT/Cargo.toml"
PIXI_TOML="$ROOT/pixi.toml"
RECIPE_YAML="$ROOT/recipe/recipe.yaml"

today_prefix() {
  local year month day mmdd
  year="$(date +%Y)"
  month=$((10#$(date +%m)))
  day=$((10#$(date +%d)))
  mmdd=$((month * 100 + day))
  echo "${year}.${mmdd}"
}

read_workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_sec = 1; next }
    /^\[/ { in_sec = 0 }
    in_sec && /^version =/ {
      line = $0
      sub(/^version = "/, "", line)
      sub(/"$/, "", line)
      print line
      exit
    }
  ' "$CARGO_TOML"
}

# Convert legacy schemes to YYYY.MMDD.N when interpreting "same day".
normalize_to_mmdd_version() {
  local version="$1"
  # Already YYYY.MMDD.N
  if [[ "$version" =~ ^([0-9]{4})\.([0-9]{3,4})\.([0-9]+)$ ]]; then
    echo "$version"
    return
  fi
  # Legacy Cargo: YYYY.M.D+N
  if [[ "$version" =~ ^([0-9]{4})\.([0-9]{1,2})\.([0-9]{1,2})\+([0-9]+)$ ]]; then
    local year="${BASH_REMATCH[1]}"
    local month=$((10#${BASH_REMATCH[2]}))
    local day=$((10#${BASH_REMATCH[3]}))
    local n="${BASH_REMATCH[4]}"
    # Old scheme used N starting at 0; map 0 -> 1 for first release of the day.
    if [[ "$n" -eq 0 ]]; then
      n=1
    fi
    echo "${year}.$((month * 100 + day)).${n}"
    return
  fi
  # Legacy padded: YYYY.MM.DD.N
  if [[ "$version" =~ ^([0-9]{4})\.([0-9]{2})\.([0-9]{2})\.([0-9]+)$ ]]; then
    local year="${BASH_REMATCH[1]}"
    local month=$((10#${BASH_REMATCH[2]}))
    local day=$((10#${BASH_REMATCH[3]}))
    local n="${BASH_REMATCH[4]}"
    if [[ "$n" -eq 0 ]]; then
      n=1
    fi
    echo "${year}.$((month * 100 + day)).${n}"
    return
  fi
  echo "$version"
}

next_version() {
  local current="$1"
  local prefix="${2:?}"
  local normalized n

  normalized="$(normalize_to_mmdd_version "$current")"
  if [[ "$normalized" =~ ^([0-9]{4}\.[0-9]{3,4})\.([0-9]+)$ ]]; then
    if [[ "${BASH_REMATCH[1]}" == "$prefix" ]]; then
      n="${BASH_REMATCH[2]}"
      echo "${prefix}.$((n + 1))"
      return
    fi
  fi

  echo "${prefix}.1"
}

update_cargo_version() {
  local version="$1"
  awk -v ver="$version" '
    /^\[workspace\.package\]$/ { in_sec = 1; print; next }
    /^\[/ { in_sec = 0 }
    in_sec && /^version =/ {
      print "version = \"" ver "\""
      next
    }
    { print }
  ' "$CARGO_TOML"
}

update_pixi_version() {
  local version="$1"
  awk -v ver="$version" '
    /^\[workspace\]$/ { in_ws = 1; in_pkg = 0; print; next }
    /^\[package\]$/ { in_pkg = 1; in_ws = 0; print; next }
    /^\[/ { in_ws = 0; in_pkg = 0 }
    (in_ws || in_pkg) && /^version =/ {
      print "version = \"" ver "\""
      next
    }
    { print }
  ' "$PIXI_TOML"
}

update_recipe_version() {
  local version="$1"
  awk -v ver="$version" '
    /^context:$/ { in_ctx = 1; print; next }
    /^[^ #]/ { in_ctx = 0 }
    in_ctx && /^  version:/ {
      print "  version: " ver
      next
    }
    { print }
  ' "$RECIPE_YAML"
}

main() {
  if [[ ! -f "$CARGO_TOML" || ! -f "$PIXI_TOML" ]]; then
    echo "update-version: expected Cargo.toml and pixi.toml in $ROOT" >&2
    exit 1
  fi

  local prefix current new_version
  prefix="$(today_prefix)"
  current="$(read_workspace_version || true)"

  if [[ "${1:-}" == "--set" ]]; then
    new_version="${2:?usage: update-version.sh --set YYYY.MMDD.N}"
  else
    new_version="$(next_version "$current" "$prefix")"
  fi

  local cargo_tmp pixi_tmp recipe_tmp
  cargo_tmp="$(mktemp)"
  pixi_tmp="$(mktemp)"
  recipe_tmp="$(mktemp)"
  trap 'rm -f "$cargo_tmp" "$pixi_tmp" "$recipe_tmp"' EXIT

  update_cargo_version "$new_version" >"$cargo_tmp"
  update_pixi_version "$new_version" >"$pixi_tmp"
  mv "$cargo_tmp" "$CARGO_TOML"
  mv "$pixi_tmp" "$PIXI_TOML"
  if [[ -f "$RECIPE_YAML" ]]; then
    update_recipe_version "$new_version" >"$recipe_tmp"
    mv "$recipe_tmp" "$RECIPE_YAML"
  fi
  trap - EXIT

  # Keep Cargo.lock in sync so `cargo build --locked` works (e.g. git source builds).
  if ! cargo update \
    -p fresh-gui-protocol \
    -p fresh-gui-backend \
    -p fresh-gui-client \
    -p fresh-gui-app \
    --quiet 2>/dev/null; then
    # Fallback when cargo is too old / unavailable: rewrite workspace crate versions.
    local lock="$ROOT/Cargo.lock"
    if [[ -f "$lock" ]]; then
      awk -v ver="$new_version" '
        /^name = "(fresh-gui-protocol|fresh-gui-backend|fresh-gui-client|fresh-gui-app)"$/ {
          print
          in_pkg = 1
          next
        }
        in_pkg && /^version = "/ {
          print "version = \"" ver "\""
          in_pkg = 0
          next
        }
        { in_pkg = 0; print }
      ' "$lock" >"${lock}.tmp"
      mv "${lock}.tmp" "$lock"
    fi
  fi

  if [[ "$current" == "$new_version" ]]; then
    echo "Version unchanged: ${new_version}"
  else
    echo "Version: ${current:-unset} -> ${new_version}"
  fi
}

main "$@"
