#!/usr/bin/env bash
# Bump project version in Cargo.toml, pixi.toml, recipe/recipe.yaml (if present),
# and the WiX-safe Tauri bundle version in crates/fresh-gui-desktop/.
#
# Scheme (Cargo-compatible SemVer + date sense):
#   YYYY.MMDD.N   e.g. 2026.630.1  (2026-06-30, first release that day)
#
# MMDD is month*100+day (no leading zeros; unique per calendar day).
# Jan–Sep yield 3 digits (e.g. 728); Oct–Dec yield 4 (e.g. 1231).
# N starts at 1 on the first bump of a calendar day; further runs the same day increment N.
#
# Windows / WiX ProductVersion mapping (major,minor ≤ 255; build ≤ 65535):
#   YYYY.MMDD.N → (YYYY-2000).(MMDD/100).( (MMDD%100)*1000 + N )
#   e.g. 2026.728.1 → 26.7.28001

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_TOML="$ROOT/Cargo.toml"
PIXI_TOML="$ROOT/pixi.toml"
RECIPE_YAML="$ROOT/recipe/recipe.yaml"
TAURI_CONF="$ROOT/crates/fresh-gui-desktop/tauri.conf.json"
DESKTOP_PKG="$ROOT/crates/fresh-gui-desktop/package.json"

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

# Parse YYYY.MMDD.N into year/mmdd/n and validate the date encoding.
# Sets: CALVER_YEAR CALVER_MMDD CALVER_N CALVER_MONTH CALVER_DAY
parse_calver() {
  local version="$1"
  if [[ ! "$version" =~ ^([0-9]{4})\.([0-9]{3,4})\.([0-9]+)$ ]]; then
    return 1
  fi
  local year="${BASH_REMATCH[1]}"
  local mmdd="${BASH_REMATCH[2]}"
  local n="${BASH_REMATCH[3]}"

  # Reject leading zeros / padded forms (e.g. 0728, 072, 001).
  if [[ "$mmdd" != "$((10#$mmdd))" ]]; then
    return 1
  fi
  if (( 10#$n < 1 )); then
    return 1
  fi

  local month=$((10#$mmdd / 100))
  local day=$((10#$mmdd % 100))
  if (( month < 1 || month > 12 || day < 1 || day > 31 )); then
    return 1
  fi

  CALVER_YEAR="$year"
  CALVER_MMDD="$mmdd"
  CALVER_N="$n"
  CALVER_MONTH="$month"
  CALVER_DAY="$day"
  return 0
}

# Convert legacy schemes to YYYY.MMDD.N when interpreting "same day".
normalize_to_mmdd_version() {
  local version="$1"
  # Already YYYY.MMDD.N (strict: valid month/day, no leading zeros)
  if parse_calver "$version"; then
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
  if parse_calver "$normalized"; then
    if [[ "${CALVER_YEAR}.${CALVER_MMDD}" == "$prefix" ]]; then
      n="$CALVER_N"
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

# Map CalVer YYYY.MMDD.N → WiX-safe major.minor.build
calver_to_wix() {
  local version="$1"
  if ! parse_calver "$version"; then
    echo "update-version: calver_to_wix expected YYYY.MMDD.N (MMDD=month*100+day, no leading zeros; month 1-12, day 1-31), got: $version" >&2
    return 1
  fi
  local major=$((10#$CALVER_YEAR - 2000))
  local month="$CALVER_MONTH"
  local patch=$((CALVER_DAY * 1000 + 10#$CALVER_N))
  if (( major < 0 || major > 255 || month < 1 || month > 12 || patch < 1 || patch > 65535 )); then
    echo "update-version: WiX mapping out of range for $version → ${major}.${month}.${patch}" >&2
    return 1
  fi
  echo "${major}.${month}.${patch}"
}

update_tauri_bundle_version() {
  local wix_ver="$1"
  local conf_tmp pkg_tmp
  conf_tmp="$(mktemp)"
  pkg_tmp="$(mktemp)"
  # First "version" key in tauri.conf.json is the app/bundle version.
  awk -v ver="$wix_ver" '
    BEGIN { done = 0 }
    !done && /"version":/ {
      sub(/"version": "[^"]*"/, "\"version\": \"" ver "\"")
      done = 1
    }
    { print }
  ' "$TAURI_CONF" >"$conf_tmp"
  mv "$conf_tmp" "$TAURI_CONF"

  awk -v ver="$wix_ver" '
    BEGIN { done = 0 }
    !done && /"version":/ {
      sub(/"version": "[^"]*"/, "\"version\": \"" ver "\"")
      done = 1
    }
    { print }
  ' "$DESKTOP_PKG" >"$pkg_tmp"
  mv "$pkg_tmp" "$DESKTOP_PKG"
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
    if ! parse_calver "$new_version"; then
      echo "update-version: --set expects YYYY.MMDD.N (MMDD=month*100+day, no leading zeros; month 1-12, day 1-31), got: $new_version" >&2
      exit 1
    fi
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

  local wix_version
  wix_version="$(calver_to_wix "$new_version")"
  if [[ -f "$TAURI_CONF" && -f "$DESKTOP_PKG" ]]; then
    update_tauri_bundle_version "$wix_version"
  fi

  # Keep Cargo.lock in sync so `cargo build --locked` works (e.g. git source builds).
  if ! cargo update \
    -p fresh-gui-protocol \
    -p fresh-gui-backend \
    -p fresh-gui-client \
    -p fresh-gui-app \
    -p fresh-gui-desktop \
    --quiet 2>/dev/null; then
    # Fallback when cargo is too old / unavailable: rewrite workspace crate versions.
    local lock="$ROOT/Cargo.lock"
    if [[ -f "$lock" ]]; then
      awk -v ver="$new_version" '
        /^name = "(fresh-gui-protocol|fresh-gui-backend|fresh-gui-client|fresh-gui-app|fresh-gui-desktop)"$/ {
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
    echo "Version unchanged: ${new_version} (WiX/bundle: ${wix_version})"
  else
    echo "Version: ${current:-unset} -> ${new_version} (WiX/bundle: ${wix_version})"
  fi
}

main "$@"
