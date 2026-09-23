#!/bin/sh
# Offline checks for Linux release selection. Run: sh scripts/test-install.sh
set -eu
installer=$(CDPATH= cd -- "$(dirname "$0")" && pwd)/install.sh
scratch=$(mktemp -d "${TMPDIR:-/tmp}/fresh-gui-install-test.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$scratch/bin"
cat > "$scratch/bin/ldd" <<'SH'
#!/bin/sh
printf '%s\n' "$TEST_LDD_OUTPUT"
SH
chmod 755 "$scratch/bin/ldd"

check_plan() {
  description=$1
  ldd_output=$2
  components=$3
  expected=$4
  output=$(PATH="$scratch/bin:$PATH" TEST_LDD_OUTPUT="$ldd_output" FRESH_GUI_OS=Linux FRESH_GUI_ARCH=x86_64 FRESH_GUI_VERSION=2026.923.2 FRESH_GUI_COMPONENTS="$components" FRESH_GUI_DRY_RUN=1 sh "$installer" 2>&1)
  case "$output" in
    *"$expected"*) ;;
    *) printf 'FAIL: %s\n%s\n' "$description" "$output" >&2; exit 1 ;;
  esac
  printf 'PASS: %s\n' "$description"
}

check_plan 'glibc 2.30 selects musl daemon' 'ldd (GNU libc) 2.30' both 'x86_64-unknown-linux-musl.tar.gz'
check_plan 'glibc 2.35 skips client' 'ldd (GNU libc) 2.35' both 'requires glibc >= 2.39'
check_plan 'glibc 2.35 keeps GNU daemon' 'ldd (GNU libc) 2.35' both 'fresh-gui-2026.923.2-x86_64-unknown-linux-gnu.tar.gz'
check_plan 'glibc 2.39 installs GNU client' 'ldd (GNU libc) 2.39' both 'fresh-gui-client-2026.923.2-x86_64-unknown-linux-gnu.tar.gz'
check_plan 'musl ldd selects musl daemon' 'musl libc (x86_64)' daemon 'x86_64-unknown-linux-musl.tar.gz'
if PATH="$scratch/bin:$PATH" TEST_LDD_OUTPUT='ldd (GNU libc) 2.35' FRESH_GUI_OS=Linux FRESH_GUI_ARCH=x86_64 FRESH_GUI_VERSION=2026.923.2 FRESH_GUI_COMPONENTS=client FRESH_GUI_DRY_RUN=1 sh "$installer" >"$scratch/client.out" 2>&1; then
  echo 'FAIL: incompatible client-only install succeeded' >&2
  exit 1
fi
printf 'PASS: incompatible client-only install fails\n'

# A failed final copy must never be reported as an installed binary.
mkdir -p "$scratch/release/bin"
printf '#!/bin/sh\n' > "$scratch/release/bin/fresh-gui"
tar -czf "$scratch/daemon.tar.gz" -C "$scratch/release" bin
cat > "$scratch/bin/curl" <<'SH'
#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    -w) shift 2 ;;
    *) url=$1; shift ;;
  esac
done
case "$url" in
  *.sha256) printf 404 ;;
  *) /bin/cp "$TEST_ARCHIVE" "$out"; printf 200 ;;
esac
SH
cat > "$scratch/bin/cp" <<'SH'
#!/bin/sh
echo 'cp: Disk quota exceeded' >&2
exit 1
SH
chmod 755 "$scratch/bin/curl" "$scratch/bin/cp"
if PATH="$scratch/bin:$PATH" TEST_ARCHIVE="$scratch/daemon.tar.gz" FRESH_GUI_OS=Linux FRESH_GUI_ARCH=x86_64 FRESH_GUI_LIBC=musl FRESH_GUI_VERSION=2026.923.2 FRESH_GUI_COMPONENTS=daemon FRESH_GUI_HOME="$scratch/home" FRESH_GUI_NO_PATH_UPDATE=1 sh "$installer" >"$scratch/quota.out" 2>&1; then
  echo 'FAIL: disk quota copy failure returned success' >&2
  exit 1
fi
if grep -q 'Installed ' "$scratch/quota.out" || [ -e "$scratch/home/bin/fresh-gui.partial" ]; then
  echo 'FAIL: failed copy was claimed as installed or left a partial file' >&2
  cat "$scratch/quota.out" >&2
  exit 1
fi
if ! grep -q 'Free space or change FRESH_GUI_HOME' "$scratch/quota.out"; then
  echo 'FAIL: disk quota guidance was missing' >&2
  cat "$scratch/quota.out" >&2
  exit 1
fi
printf 'PASS: disk quota copy failure is fatal and cleans partial\n'
