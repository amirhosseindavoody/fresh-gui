#!/bin/sh
# fresh-gui installer (Linux, and Git Bash / MSYS on Windows).
#
#   curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | sh
#
# Downloads the GPUI host and the headless daemon for this machine from GitHub
# Releases, checks the sibling .sha256 asset when it exists, and copies them
# into ~/.fresh-gui/bin. The command on PATH is `fresh-gui` (the desktop app).
# When both archives are installed, the headless daemon is `fresh-gui-daemon`
# beside it. A daemon-only install keeps the headless binary named `fresh-gui`.
# Adds that directory to the shell rc / profile unless FRESH_GUI_NO_PATH_UPDATE
# is set.
# With the default components=both, a release that lacks one of the two
# archives still installs the other. components=client or daemon fails if
# that archive is missing.
#
# Environment:
#   FRESH_GUI_VERSION         latest (default) or CalVer (2026.921.5 / v2026.921.5)
#   FRESH_GUI_HOME            install prefix (default: $HOME/.fresh-gui)
#   FRESH_GUI_BIN_DIR         binaries directory (default: $FRESH_GUI_HOME/bin)
#   FRESH_GUI_REPOURL         GitHub repository (default: the fresh-gui repo)
#   FRESH_GUI_NO_PATH_UPDATE  any non-empty value skips PATH / rc edits
#   FRESH_GUI_COMPONENTS      both (default) | client | daemon
#   FRESH_GUI_LIBC            gnu (default) | musl  — Linux daemon libc.
#                             Alpine is detected as musl. The GPUI client is
#                             published for gnu only.
#   FRESH_GUI_ARCH            override uname -m (only x86_64 is published)
#   FRESH_GUI_OS              override uname -s (Linux, Darwin, MINGW*, MSYS*)
#   FRESH_GUI_DRY_RUN         any non-empty value prints the plan and exits
#   NETRC                     optional curl/wget netrc file for a private repo
#
# shellcheck shell=sh

set -eu

main() {
  version="${FRESH_GUI_VERSION:-latest}"
  home_dir="${FRESH_GUI_HOME:-${HOME:-}/.fresh-gui}"
  case "$home_dir" in
    '~' | '~'/*) home_dir="${HOME:-}${home_dir#\~}" ;;
  esac
  if [ -z "${HOME:-}" ] && [ -z "${FRESH_GUI_HOME:-}" ]; then
    echo "error: HOME is not set. Set HOME or FRESH_GUI_HOME." >&2
    exit 1
  fi
  bin_dir="${FRESH_GUI_BIN_DIR:-$home_dir/bin}"
  repo="${FRESH_GUI_REPOURL:-https://github.com/amirhosseindavoody/fresh-gui}"
  repo="${repo%/}"

  components=$(printf '%s' "${FRESH_GUI_COMPONENTS:-both}" | tr '[:upper:]' '[:lower:]')
  want_client=0
  want_daemon=0
  case "$components" in
    both | all | '')
      want_client=1
      want_daemon=1
      ;;
    client | app | gui)
      want_client=1
      ;;
    daemon | server | backend)
      want_daemon=1
      ;;
    *)
      echo "error: FRESH_GUI_COMPONENTS must be both, client, or daemon (got '${components}')." >&2
      exit 1
      ;;
  esac

  os=$(detect_os) || exit 1
  arch=$(detect_arch) || exit 1
  libc=$(detect_libc) || exit 1
  glibc_version=$(detect_glibc_version)
  if [ "$os" = Linux ] && [ "$want_client" -eq 1 ] && [ "$libc" = gnu ] && version_lt "$glibc_version" 2.39; then
    if [ "$want_daemon" -eq 1 ]; then
      echo "note: the GPUI client requires glibc >= 2.39 (detected ${glibc_version:-unknown}); skipping the client." >&2
      want_client=0
    else
      echo "error: the GPUI client requires glibc >= 2.39 (detected ${glibc_version:-unknown})." >&2
      exit 1
    fi
  fi
  target=$(select_target "$os" "$arch" "$libc") || exit 1
  ext=$(archive_ext "$target")

  if [ "$libc" = "musl" ] && [ "$want_client" -eq 1 ]; then
    if [ "$want_daemon" -eq 1 ]; then
      echo "note: the GPUI client is published for x86_64-unknown-linux-gnu only." >&2
      echo "      Skipping the desktop command. Set FRESH_GUI_LIBC=gnu to download the glibc client." >&2
      echo "      Installing the musl daemon as fresh-gui." >&2
      want_client=0
    else
      echo "error: the GPUI client is published for x86_64-unknown-linux-gnu only." >&2
      echo "       This install asked for the client with libc=musl." >&2
      echo "       Set FRESH_GUI_LIBC=gnu, or FRESH_GUI_COMPONENTS=daemon for the musl daemon." >&2
      exit 1
    fi
  fi

  case "$version" in
    latest)
      version=$(resolve_latest_version "$repo") || exit 1
      ;;
  esac
  version=${version#v}
  case "$version" in
    '' | *[!A-Za-z0-9._+-]*)
      echo "error: invalid FRESH_GUI_VERSION '${version}'." >&2
      exit 1
      ;;
  esac

  client_url=""
  daemon_url=""
  if [ "$want_client" -eq 1 ]; then
    client_url="${repo}/releases/download/v${version}/fresh-gui-client-${version}-${target}.${ext}"
  fi
  if [ "$want_daemon" -eq 1 ]; then
    daemon_url="${repo}/releases/download/v${version}/fresh-gui-${version}-${target}.${ext}"
  fi

  printf 'This script will download and install fresh-gui (%s).\n' "$version"
  printf 'Binaries will be installed into %s\n' "$bin_dir"
  if [ -n "$client_url" ]; then
    printf 'Client: %s\n' "$(mask_credentials "$client_url")"
  fi
  if [ -n "$daemon_url" ]; then
    printf 'Daemon: %s\n' "$(mask_credentials "$daemon_url")"
  fi

  if [ -n "${FRESH_GUI_DRY_RUN:-}" ]; then
    echo "Dry run: no files will be written."
    exit 0
  fi

  work=$(mktemp -d "${TMPDIR:-/tmp}/fresh-gui-install.XXXXXXXX")
  trap 'rm -rf "$work"' EXIT INT TERM

  # Default installs both when the release has both archives. A 404 on one
  # of them skips that piece. Asking for only client or only daemon fails
  # if that archive is missing.
  optional=0
  if [ "$want_client" -eq 1 ] && [ "$want_daemon" -eq 1 ]; then
    optional=1
  fi
  got_client=0
  got_daemon=0
  # Package names inside older archives varied; the installed command is always fresh-gui.
  if [ "$want_client" -eq 1 ]; then
    if [ "$ext" = "zip" ]; then
      if install_named "$client_url" "$work/client" "fresh-gui.exe" "$bin_dir" "$ext" "$optional" \
        fresh-gui-app.exe fresh-gui.exe; then
        got_client=1
      fi
    elif install_named "$client_url" "$work/client" "fresh-gui" "$bin_dir" "$ext" "$optional" \
      fresh-gui-app fresh-gui; then
      got_client=1
    fi
  fi
  if [ "$got_client" -eq 1 ]; then
    # Remove the compatibility name left by older installer versions.
    rm -f "${bin_dir}/fresh-gui-app" "${bin_dir}/fresh-gui-app.exe"
  fi
  if [ "$want_daemon" -eq 1 ]; then
    # Beside the desktop command the daemon cannot also be named fresh-gui.
    # Daemon-only (servers, musl) keeps the historical `fresh-gui` name so
    # `remote connect` and pixi-style headless use still match.
    if [ "$got_client" -eq 1 ]; then
      if [ "$ext" = "zip" ]; then
        daemon_dest="fresh-gui-daemon.exe"
      else
        daemon_dest="fresh-gui-daemon"
      fi
    elif [ "$ext" = "zip" ]; then
      daemon_dest="fresh-gui.exe"
    else
      daemon_dest="fresh-gui"
    fi
    if [ "$ext" = "zip" ]; then
      if install_named "$daemon_url" "$work/daemon" "$daemon_dest" "$bin_dir" "$ext" "$optional" \
        fresh-gui.exe fresh-gui-daemon.exe; then
        got_daemon=1
      fi
    elif install_named "$daemon_url" "$work/daemon" "$daemon_dest" "$bin_dir" "$ext" "$optional" \
      fresh-gui fresh-gui-daemon; then
      got_daemon=1
    fi
  fi
  if [ "$got_client" -eq 0 ] && [ "$got_daemon" -eq 0 ]; then
    echo "error: no fresh-gui archive from this release could be installed." >&2
    exit 1
  fi

  echo "Installed into '${bin_dir}'."

  if [ -n "${FRESH_GUI_NO_PATH_UPDATE:-}" ]; then
    echo "No PATH update because FRESH_GUI_NO_PATH_UPDATE is set."
    echo "Add '${bin_dir}' to your PATH to run fresh-gui."
  else
    update_path "$bin_dir"
  fi

  print_next_steps "$got_client" "$got_daemon" "$os"
}

detect_os() {
  os_name=${FRESH_GUI_OS:-$(uname -s)}
  case "$os_name" in
    Linux | linux) printf '%s\n' Linux ;;
    Darwin | darwin | macOS) printf '%s\n' Darwin ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT | Windows | windows) printf '%s\n' Windows ;;
    *)
      echo "error: unsupported operating system '${os_name}'." >&2
      echo "       fresh-gui publishes Linux x86_64 and Windows x86_64 binaries." >&2
      exit 1
      ;;
  esac
}

detect_arch() {
  machine=${FRESH_GUI_ARCH:-$(uname -m)}
  case "$machine" in
    x86_64 | amd64 | x64) printf '%s\n' x86_64 ;;
    *)
      echo "error: unsupported architecture '${machine}'." >&2
      echo "       fresh-gui publishes x86_64 binaries (Linux gnu/musl and Windows msvc)." >&2
      exit 1
      ;;
  esac
}

detect_libc() {
  if [ -n "${FRESH_GUI_LIBC:-}" ]; then
    case "$FRESH_GUI_LIBC" in
      gnu | glibc) printf '%s\n' gnu ;;
      musl) printf '%s\n' musl ;;
      *)
        echo "error: FRESH_GUI_LIBC must be gnu or musl (got '${FRESH_GUI_LIBC}')." >&2
        exit 1
        ;;
    esac
    return
  fi
  if [ -f /etc/alpine-release ]; then
    printf '%s\n' musl
    return
  fi
  glibc_version=$(detect_glibc_version)
  if [ -n "$glibc_version" ] && version_lt "$glibc_version" 2.31; then
    printf '%s\n' musl
    return
  fi
  if command -v ldd >/dev/null 2>&1; then
    ldd_out=$(ldd --version 2>&1 || true)
    case "$ldd_out" in
      *[Mm]usl*)
        printf '%s\n' musl
        return
        ;;
    esac
  fi
  printf '%s\n' gnu
}

# Parse glibc's own version from ldd output. On systems without ldd or where
# its output is not recognizable, return empty so explicit libc selection can
# still be used and we do not invent a version.
detect_glibc_version() {
  command -v ldd >/dev/null 2>&1 || return 0
  ldd_out=$(ldd --version 2>&1 || true)
  printf '%s\n' "$ldd_out" | sed -nE 's/.*[Gg][Ll][Ii][Bb][Cc][^0-9]*([0-9]+\.[0-9]+).*/\1/p; t; s/.*[^0-9]([0-9]+\.[0-9]+)([^0-9].*)?$/\1/p' | awk 'NR == 1 { print; exit }'
}

version_lt() {
  [ -n "$1" ] || return 1
  awk -v a="$1" -v b="$2" 'BEGIN { split(a,x,"."); split(b,y,"."); exit !((x[1]+0 < y[1]+0) || (x[1]+0 == y[1]+0 && x[2]+0 < y[2]+0)) }'
}

select_target() {
  os_name=$1
  machine=$2
  libc_name=$3
  case "$os_name" in
    Linux)
      case "$machine" in
        x86_64)
          case "$libc_name" in
            gnu)
              printf '%s\n' x86_64-unknown-linux-gnu
              return
              ;;
            musl)
              printf '%s\n' x86_64-unknown-linux-musl
              return
              ;;
          esac
          ;;
      esac
      ;;
    Darwin)
      echo "error: macOS is not a published fresh-gui target." >&2
      echo "       Install the Linux or Windows client and connect it to a Linux daemon." >&2
      exit 1
      ;;
    Windows)
      case "$machine" in
        x86_64)
          printf '%s\n' x86_64-pc-windows-msvc
          return
          ;;
      esac
      ;;
  esac
  echo "error: no fresh-gui release for ${os_name} ${machine} (${libc_name})." >&2
  echo "       Published targets: Linux x86_64 (gnu client + daemon, musl daemon) and Windows x86_64." >&2
  exit 1
}

archive_ext() {
  case "$1" in
    *windows*) printf '%s\n' zip ;;
    *) printf '%s\n' tar.gz ;;
  esac
}

resolve_latest_version() {
  repo_url=$1
  latest_url="${repo_url}/releases/latest"
  final=""
  if command -v curl >/dev/null 2>&1; then
    final=$(run_curl -fsSL -o /dev/null -w '%{url_effective}' "$latest_url" || true)
  elif command -v wget >/dev/null 2>&1; then
    log="${TMPDIR:-/tmp}/fresh-gui-latest.$$"
    if [ -n "${NETRC:-}" ]; then
      wget -S -O /dev/null --netrc-file="$NETRC" "$latest_url" >"$log" 2>&1 || true
    elif [ -n "${HOME:-}" ] && [ -f "$HOME/.netrc" ]; then
      wget -S -O /dev/null --netrc "$latest_url" >"$log" 2>&1 || true
    else
      wget -S -O /dev/null "$latest_url" >"$log" 2>&1 || true
    fi
    final=$(tr -d '\r' <"$log" | awk 'tolower($1) == "location:" { url=$2 } END { print url }')
    rm -f "$log"
  else
    echo "error: curl or wget is required." >&2
    exit 1
  fi
  tag=${final##*/}
  case "$tag" in
    v*)
      printf '%s\n' "${tag#v}"
      ;;
    *)
      echo "error: could not resolve the latest release from ${latest_url}." >&2
      echo "       Last URL: ${final:-<empty>}" >&2
      echo "       Set FRESH_GUI_VERSION to a tag such as 2026.921.5." >&2
      exit 1
      ;;
  esac
}

mask_credentials() {
  printf '%s\n' "$1" | sed -e 's|://[^:@/][^:@/]*:[^@/][^@/]*@|://***:***@|g'
}

run_curl() {
  if [ -n "${NETRC:-}" ]; then
    curl --netrc-file "$NETRC" "$@"
  elif [ -n "${HOME:-}" ] && [ -f "$HOME/.netrc" ]; then
    curl --netrc "$@"
  else
    curl "$@"
  fi
}

run_wget() {
  if [ -n "${NETRC:-}" ]; then
    wget --netrc-file="$NETRC" "$@"
  elif [ -n "${HOME:-}" ] && [ -f "$HOME/.netrc" ]; then
    wget --netrc "$@"
  else
    wget "$@"
  fi
}

# Download $1 to $2. Prints the HTTP status on stdout. Returns 0 always
# (callers inspect the status) so set -e does not abort on a 404.
http_download() {
  url=$1
  dest=$2
  code=""
  if command -v curl >/dev/null 2>&1; then
    # stderr stays on the terminal when the script is piped to sh; stdout is
    # captured by the caller, so progress follows stderr, not stdout.
    if [ -t 2 ]; then
      code=$(run_curl -# -S -L --retry 3 --retry-delay 1 -o "$dest" -w '%{http_code}' "$url" || true)
    else
      code=$(run_curl -sS -L --retry 3 --retry-delay 1 -o "$dest" -w '%{http_code}' "$url" || true)
    fi
  elif command -v wget >/dev/null 2>&1; then
    log=$(mktemp "${TMPDIR:-/tmp}/fresh-gui-wget.XXXXXXXX")
    if run_wget -S -O "$dest" "$url" >"$log" 2>&1; then
      code=200
    else
      code=$(tr -d '\r' <"$log" | awk '/HTTP\// { c=$2 } END { print c }')
      if [ -z "$code" ]; then
        code=000
      fi
    fi
    rm -f "$log"
  else
    echo "error: curl or wget is required." >&2
    exit 1
  fi
  code=$(printf '%s' "$code" | tr -d '[:space:]')
  if [ -z "$code" ]; then
    code=000
  fi
  printf '%s\n' "$code"
}

is_sha256() {
  value=$1
  case "$value" in
    *[!0-9a-f]*) return 1 ;;
  esac
  [ "$(printf '%s' "$value" | wc -c | tr -d '[:space:]')" -eq 64 ]
}

sha256_file() {
  file=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk 'NR == 1 { print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk 'NR == 1 { print $1 }'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | awk '{ print $NF }'
  else
    echo "error: sha256sum, shasum, or openssl is required to verify checksums." >&2
    exit 1
  fi
}

verify_checksum() {
  archive=$1
  sum_url=$2
  sum_file=$3
  code=$(http_download "$sum_url" "$sum_file") || exit 1
  case "$code" in
    404)
      echo "warning: no checksum asset ($(mask_credentials "$sum_url")); continuing without verification." >&2
      return 0
      ;;
    2*) ;;
    *)
      echo "error: checksum download failed (HTTP ${code}): $(mask_credentials "$sum_url")" >&2
      exit 1
      ;;
  esac
  if [ ! -s "$sum_file" ]; then
    echo "error: checksum file is empty: $(mask_credentials "$sum_url")" >&2
    exit 1
  fi
  expected=$(tr -d '\r' <"$sum_file" | awk 'NF { print $1; exit }' | tr '[:upper:]' '[:lower:]')
  if ! is_sha256 "$expected"; then
    echo "error: checksum file does not start with a SHA-256 hex digest: $(mask_credentials "$sum_url")" >&2
    exit 1
  fi
  actual=$(sha256_file "$archive") || exit 1
  actual=$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')
  if [ "$actual" != "$expected" ]; then
    echo "error: checksum mismatch for $(mask_credentials "$sum_url")" >&2
    echo "       expected ${expected}" >&2
    echo "       actual   ${actual}" >&2
    exit 1
  fi
  echo "Checksum verified."
}

# Current Windows release assets are GNU tar archives named .zip (the
# package script falls back to `tar -a` when Info-ZIP is absent, and that
# produces a tar file). Real PK zips still extract with unzip.
is_zip_file() {
  file=$1
  if ! command -v od >/dev/null 2>&1; then
    return 1
  fi
  sig=$(od -An -N 2 -t x1 "$file" 2>/dev/null | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
  [ "$sig" = "504b" ]
}

extract_archive() {
  archive=$1
  dest=$2
  ext_name=$3
  mkdir -p "$dest"
  case "$ext_name" in
    tar.gz)
      if ! command -v tar >/dev/null 2>&1; then
        echo "error: tar is required to extract the release archive." >&2
        exit 1
      fi
      tar -xzf "$archive" -C "$dest"
      ;;
    zip)
      if is_zip_file "$archive" && command -v unzip >/dev/null 2>&1; then
        unzip -q -o "$archive" -d "$dest"
      elif command -v tar >/dev/null 2>&1; then
        tar -xf "$archive" -C "$dest"
      elif command -v unzip >/dev/null 2>&1; then
        unzip -q -o "$archive" -d "$dest"
      else
        echo "error: tar or unzip is required to extract the archive." >&2
        exit 1
      fi
      ;;
    *)
      echo "error: unknown archive type '${ext_name}'." >&2
      exit 1
      ;;
  esac
}

# install_named URL STAGE DEST_BASENAME DEST_DIR EXT OPTIONAL CANDIDATE...
# Copies the first archive member whose file name matches a candidate.
install_named() {
  url=$1
  stage=$2
  dest_base=$3
  dest_dir=$4
  ext_name=$5
  optional=$6
  shift 6
  mkdir -p "$stage"
  archive="$stage/archive"
  code=$(http_download "$url" "$archive") || exit 1
  case "$code" in
    2*) ;;
    404)
      if [ "$optional" -eq 1 ]; then
        echo "note: not in this release, skipping: $(mask_credentials "$url")" >&2
        return 1
      fi
      echo "error: download failed (HTTP 404): $(mask_credentials "$url")" >&2
      exit 1
      ;;
    *)
      echo "error: download failed (HTTP ${code}): $(mask_credentials "$url")" >&2
      exit 1
      ;;
  esac
  if [ ! -s "$archive" ]; then
    echo "error: downloaded file is empty: $(mask_credentials "$url")" >&2
    exit 1
  fi
  verify_checksum "$archive" "${url}.sha256" "$stage/archive.sha256"
  extract_archive "$archive" "$stage/extract" "$ext_name"
  src=""
  for name in "$@"; do
    found=$(find "$stage/extract" -type f -name "$name" | awk 'NR == 1 { print; exit }')
    if [ -n "$found" ]; then
      src=$found
      break
    fi
  done
  if [ -z "$src" ]; then
    echo "error: archive does not contain any of: $*" >&2
    exit 1
  fi
  mkdir -p "$dest_dir"
  tmp_dest="${dest_dir}/${dest_base}.partial"
  if ! cp "$src" "$tmp_dest"; then
    rm -f "$tmp_dest"
    echo "error: could not copy the binary into '${dest_dir}' (possibly disk quota exceeded). Free space or change FRESH_GUI_HOME, then retry." >&2
    exit 1
  fi
  if ! chmod 755 "$tmp_dest" || ! mv -f "$tmp_dest" "${dest_dir}/${dest_base}"; then
    rm -f "$tmp_dest"
    echo "error: could not install binary into '${dest_dir}'. Free space or change FRESH_GUI_HOME, then retry." >&2
    exit 1
  fi
  echo "Installed ${dest_dir}/${dest_base}"
}

update_shell_file() {
  file=$1
  line=$2
  dir=$(dirname "$file")
  mkdir -p "$dir"
  if [ ! -f "$file" ]; then
    touch "$file"
  fi
  if ! grep -Fqx "$line" "$file"; then
    printf '\n%s\n' "$line" >>"$file"
    echo "Updating '${file}'."
    path_updated=1
  fi
}

update_path() {
  dest_dir=$1
  path_updated=0
  shell_name=$(basename "${SHELL:-}")
  case "$shell_name" in
    bash)
      update_shell_file "${HOME}/.bashrc" "export PATH=\"${dest_dir}:\$PATH\""
      update_shell_file "${HOME}/.profile" "export PATH=\"${dest_dir}:\$PATH\""
      ;;
    zsh)
      update_shell_file "${HOME}/.zshrc" "export PATH=\"${dest_dir}:\$PATH\""
      ;;
    fish)
      update_shell_file "${HOME}/.config/fish/config.fish" "set -gx PATH \"${dest_dir}\" \$PATH"
      ;;
    tcsh)
      update_shell_file "${HOME}/.tcshrc" "set path = ( ${dest_dir} \$path )"
      ;;
    sh | dash)
      update_shell_file "${HOME}/.profile" "export PATH=\"${dest_dir}:\$PATH\""
      ;;
    '')
      echo "warning: could not detect the shell. Add '${dest_dir}' to your PATH." >&2
      ;;
    *)
      echo "warning: could not update shell '${shell_name}'. Add '${dest_dir}' to your PATH." >&2
      ;;
  esac
  if [ "$path_updated" -eq 1 ]; then
    echo "Restart your shell, or source the updated rc file, before using fresh-gui."
  else
    case "$shell_name" in
      bash | zsh | fish | tcsh | sh | dash)
        echo "'${dest_dir}' is already on the PATH entry in your shell rc."
        ;;
    esac
  fi
}

print_next_steps() {
  did_client=$1
  did_daemon=$2
  os_name=$3
  echo
  if [ "$did_client" -eq 1 ]; then
    echo "Run fresh-gui. It starts the local daemon when needed and opens the window:"
    echo "  fresh-gui"
    echo "  fresh-gui /path/to/project"
    echo "Connect to a Linux machine over SSH:"
    echo "  fresh-gui user@server"
    echo "  fresh-gui remote add lab user@server --root /path/to/project"
    echo "  fresh-gui remote connect lab"
  elif [ "$did_daemon" -eq 1 ]; then
    echo "Daemon-only install. Start a background session in a project directory:"
    echo "  cd /path/to/project"
    echo "  fresh-gui"
    echo "  fresh-gui close"
  fi
  if [ "$did_client" -eq 1 ] && [ "$os_name" = "Linux" ]; then
    echo
    echo "The Linux client needs glibc >= 2.39, an X11 or Wayland session, fontconfig, and libvulkan.so.1."
    echo "  sudo apt install libvulkan1 libfontconfig1 libfreetype6 libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libxcb1"
  fi
}

main "$@"
