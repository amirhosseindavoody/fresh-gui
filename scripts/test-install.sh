#!/bin/sh
# Offline checks for Linux release selection. Run: sh scripts/test-install.sh
set -eu
installer=$(CDPATH= cd -- "$(dirname "$0")" && pwd)/install.sh
scratch=$(mktemp -d "${TMPDIR:-/tmp}/fresh-gui-install-test.XXXXXXXX")
cleanup() {
  if [ -n "${TEST_DAEMON_PID:-}" ]; then kill "$TEST_DAEMON_PID" 2>/dev/null || true; fi
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM
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

# A live local daemon must be explicitly stopped before its binary is replaced.
cat > "$scratch/bin/cp" <<'SH'
#!/bin/sh
exec /bin/cp "$@"
SH
chmod 755 "$scratch/bin/cp"
mkdir -p "$scratch/stop-home/bin"
cat > "$scratch/stop-home/bin/fresh-gui" <<'SH'
#!/bin/sh
case "$1" in
  close)
    kill "$TEST_DAEMON_PID"
    rm -f "$TEST_SESSION_FILE"
    printf 'Stopped the local daemon.\n'
    ;;
esac
SH
chmod 755 "$scratch/stop-home/bin/fresh-gui"
export PATH="$scratch/bin:$PATH" TEST_ARCHIVE="$scratch/daemon.tar.gz"
export TEST_SESSION_FILE="$scratch/runtime/fresh-gui/session.json" XDG_RUNTIME_DIR="$scratch/runtime"
export FRESH_GUI_OS=Linux FRESH_GUI_ARCH=x86_64 FRESH_GUI_LIBC=musl FRESH_GUI_VERSION=2026.923.2 FRESH_GUI_COMPONENTS=daemon
export FRESH_GUI_HOME="$scratch/stop-home" FRESH_GUI_NO_PATH_UPDATE=1
start_fake_daemon() {
  if [ -n "${TEST_DAEMON_PID:-}" ]; then
    kill "$TEST_DAEMON_PID" 2>/dev/null || true
    wait "$TEST_DAEMON_PID" 2>/dev/null || true
  fi
  sleep 300 &
  TEST_DAEMON_PID=$!
  export TEST_DAEMON_PID
  mkdir -p "$(dirname "$TEST_SESSION_FILE")"
  printf '{"pid":%s}\n' "$TEST_DAEMON_PID" > "$TEST_SESSION_FILE"
}
start_fake_daemon
if sh "$installer" </dev/null >"$scratch/no-tty.out" 2>&1; then
  echo 'FAIL: running daemon update without a console succeeded' >&2
  exit 1
fi
if [ ! -f "$TEST_SESSION_FILE" ] || ! grep -q 'FRESH_GUI_STOP_DAEMON=1' "$scratch/no-tty.out"; then
  echo 'FAIL: no-console refusal did not preserve the running daemon or explain auto-stop' >&2
  cat "$scratch/no-tty.out" >&2
  exit 1
fi
printf 'PASS: running daemon without a console fails safely\n'

start_fake_daemon
if ! FRESH_GUI_STOP_DAEMON=1 sh "$installer" >"$scratch/auto-stop.out" 2>&1; then
  echo 'FAIL: FRESH_GUI_STOP_DAEMON=1 install failed' >&2
  cat "$scratch/auto-stop.out" >&2
  exit 1
fi
if [ -f "$TEST_SESSION_FILE" ] || ! grep -q 'Stopped the local daemon' "$scratch/auto-stop.out"; then
  echo 'FAIL: FRESH_GUI_STOP_DAEMON=1 did not stop the daemon' >&2
  cat "$scratch/auto-stop.out" >&2
  exit 1
fi
printf 'PASS: FRESH_GUI_STOP_DAEMON=1 stops the daemon unattended\n'

# Reinstall the close fixture after the auto-stop case replaces it.
cat > "$scratch/stop-home/bin/fresh-gui" <<'SH'
#!/bin/sh
case "$1" in
  close)
    kill "$TEST_DAEMON_PID"
    rm -f "$TEST_SESSION_FILE"
    printf 'Stopped the local daemon.\n'
    ;;
esac
SH
chmod 755 "$scratch/stop-home/bin/fresh-gui"

if command -v script >/dev/null 2>&1; then
  start_fake_daemon
  if printf 'n\n' | script -q -e -c "sh '$installer'" /dev/null >"$scratch/no.out" 2>&1; then
    echo 'FAIL: declining to stop daemon succeeded' >&2
    cat "$scratch/no.out" >&2
    exit 1
  fi
  if [ ! -f "$TEST_SESSION_FILE" ] || [ -e "$scratch/stop-home/bin/fresh-gui.partial" ]; then
    echo 'FAIL: declined install changed the daemon or wrote a partial binary' >&2
    exit 1
  fi
  printf 'PASS: answering no leaves the running daemon and binaries untouched\n'

  start_fake_daemon
  printf 'y\n' | script -q -e -c "sh '$installer'" /dev/null >"$scratch/yes.out" 2>&1
  if [ -f "$TEST_SESSION_FILE" ] || ! grep -q 'Stopped the local daemon' "$scratch/yes.out" || [ ! -x "$scratch/stop-home/bin/fresh-gui" ]; then
    echo 'FAIL: answering yes did not stop then install' >&2
    cat "$scratch/yes.out" >&2
    exit 1
  fi
  printf 'PASS: answering yes cleanly stops the daemon before install\n'
else
  printf 'SKIP: interactive yes/no checks need the script utility\n'
fi

# A close command that exits successfully without stopping the PID is not enough.
cat > "$scratch/stop-home/bin/fresh-gui" <<'SH'
#!/bin/sh
case "$1" in close) printf 'close returned without stopping\n' ;; esac
SH
chmod 755 "$scratch/stop-home/bin/fresh-gui"
start_fake_daemon
if FRESH_GUI_STOP_DAEMON=1 sh "$installer" >"$scratch/failed-close.out" 2>&1; then
  echo 'FAIL: installer replaced a binary while the daemon PID was still running' >&2
  exit 1
fi
if [ ! -f "$TEST_SESSION_FILE" ] || ! grep -q 'still running' "$scratch/failed-close.out"; then
  echo 'FAIL: failed close did not preserve the session or explain the abort' >&2
  cat "$scratch/failed-close.out" >&2
  exit 1
fi
kill "$TEST_DAEMON_PID" 2>/dev/null || true
printf 'PASS: close must stop the daemon PID before install\n'

# With no live session, the installer proceeds without asking or stopping.
rm -f "$TEST_SESSION_FILE"
sh "$installer" >"$scratch/no-daemon.out" 2>&1
if grep -q 'A local fresh-gui daemon session is running' "$scratch/no-daemon.out"; then
  echo 'FAIL: installer prompted when no daemon was running' >&2
  exit 1
fi
printf 'PASS: no running daemon installs without prompting\n'

# A daemon-only update also removes an older client compatibility name.
cat > "$scratch/bin/cp" <<'SH'
#!/bin/sh
exec /bin/cp "$@"
SH
chmod 755 "$scratch/bin/cp"
mkdir -p "$scratch/home/bin"
printf 'old alias\n' > "$scratch/home/bin/fresh-gui-app"
PATH="$scratch/bin:$PATH" TEST_ARCHIVE="$scratch/daemon.tar.gz" FRESH_GUI_OS=Linux FRESH_GUI_ARCH=x86_64 FRESH_GUI_LIBC=musl FRESH_GUI_VERSION=2026.923.2 FRESH_GUI_COMPONENTS=daemon FRESH_GUI_HOME="$scratch/home" FRESH_GUI_NO_PATH_UPDATE=1 sh "$installer" >"$scratch/daemon.out" 2>&1
if [ -e "$scratch/home/bin/fresh-gui-app" ] || [ ! -x "$scratch/home/bin/fresh-gui" ]; then
  echo 'FAIL: daemon-only update left old client alias or missed daemon' >&2
  exit 1
fi
printf 'PASS: daemon-only update removes old client alias\n'
