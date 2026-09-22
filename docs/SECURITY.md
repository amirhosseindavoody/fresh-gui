# Secure Access: Always-On Token + SSH Tunnel

How operators reach `fresh-gui` on shared hosts. Architecture overview: [DESIGN.md](./DESIGN.md). User-facing install steps: [README.md](../README.md).

## 1. Problem

`fresh-gui` typically runs on a **shared multi-user Linux server** (a compute node, dev box, etc.), not a personal single-user machine. It exposes a real PTY (interactive shell) and file system access under the operator's own Unix permissions to whoever can complete the ADE WebSocket handshake.

`127.0.0.1` is a property of the machine's network stack, **not** of a Unix user — every other user logged into the same shared host can open a TCP connection to `127.0.0.1:7420`. An unauthenticated loopback ADE is therefore reachable by any local account on that host. Remote laptop access also needs a copy-pasteable path that does not bind the daemon publicly.

## 2. Requirements

1. **Always require a bearer token** to authenticate a session, even on the default loopback bind. No opt-out in normal operation.
2. **Never expose the backend to the network by default.** Keep the loopback bind as the default and the recommended path — remote access goes through an **SSH tunnel**, which is already authenticated (SSH login) and encrypted end to end.
3. **Zero-friction access instructions.** On startup, the process prints:
   - a **local** URL (same machine) with the token embedded, and
   - the exact **SSH tunnel command** to run on a laptop, then `fresh-gui --backend` with the Local access URL (or `fresh-gui user@host` / `remote connect`, which builds the tunnel itself).
4. Keep a narrow, explicit escape hatch for local integration tests (`--allow-no-auth`, loopback-only) so CI doesn't need to thread tokens through every test — never used in the documented user-facing flow.

## 3. Design

### 3.1 Token lifecycle

- If `--token` / `FRESH_GUI_TOKEN` is set (and non-empty), use it as-is (lets an operator pin a stable token, e.g. to reuse across restarts).
- Otherwise, **auto-generate** a random token per process start (`uuid::Uuid::new_v4()`, 122 bits of OS-RNG entropy, formatted as 32 hex chars). It is never written to git/config.
- The token is **never included in `tracing` structured logs** (only `auth_required: bool` is logged). For the background session, it is stored in the user-private session meta file (`$XDG_RUNTIME_DIR/fresh-gui/session.json` on Unix, mode `0600`; `%LOCALAPPDATA%\fresh-gui\session.json` on Windows) so a later `fresh-gui` / `fresh-gui status` can reprint the Local access URL. The desktop command reads that meta by running the daemon binary with `--json` (stdout of a local pipe). The token is not an argument of the GUI process. The meta file is removed on `fresh-gui close`.
- Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in `ps` / process listings.

### 3.2 Bind stays loopback by default

`--listen` defaults to `127.0.0.1:7420`. Non-loopback binds remain possible for advanced setups but are not the documented path; a `warn!` log fires when the bind is non-loopback, nudging the operator back to loopback + SSH tunnel.

### 3.3 Background session + startup banner

Default `fresh-gui` detaches a per-user daemon (exclusive lock), prints status, and returns the shell. Re-running `fresh-gui` reprints status; `fresh-gui close` stops the daemon (SIGTERM on Unix, `TerminateProcess` on Windows). Daemon stdout/stderr go to `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` on Unix (fallback `~/.local/state/fresh-gui/`) or `%LOCALAPPDATA%\fresh-gui\fresh-gui.log` on Windows.

```text
  fresh-gui session
  pid:  12345
  UI:   http://127.0.0.1:7420/
  WS:   ws://127.0.0.1:7420/ws
  root: /path/to/project
  log:  ~/.local/state/fresh-gui/fresh-gui.log

  Local access (this machine):
    http://127.0.0.1:7420/?token=<token>

  From another machine (e.g. your laptop) — SSH tunnel, nothing exposed to the network:
    ssh -L 7420:127.0.0.1:7420 <user>@<host>
    fresh-gui user@<host>
    fresh-gui --backend 'http://127.0.0.1:7420/?token=<token>'

  Stop with: fresh-gui close
```

`<user>` comes from `$USER` / `$LOGNAME`; `<host>` reuses the existing FQDN-detection helper (`assigned_host_domain()`) already used for the plain `UI:` / `WS:` lines, falling back to a placeholder if no domain is detectable. The port in the local / tunnel lines follows the **bound** port (after any listen fallback).

### 3.4 Native client

`fresh-gui` on a desktop install reads the local session with `fresh-gui-daemon --json` (or attaches with `--backend`, which still accepts `?token=` / `FRESH_GUI_TOKEN` / `--token`). The token stays in the client process for the WebSocket handshake. It is not written to `remotes.json`. `fresh-gui user@host` and `remote connect` read it from the remote `session.json` over SSH instead of putting it on the client argv.

### 3.5 SSH bootstrap from the native host

`fresh-gui user@host` and `fresh-gui remote connect` build the same loopback tunnel. They use the **local OpenSSH client** (`ssh` / `scp`): keys, agent, and `~/.ssh/config`. The host does not collect passwords (`BatchMode=yes`). Host-key checking stays at OpenSSH defaults.

The ADE token is read from the remote user's private `session.json` over that SSH session and kept in the client process for the WebSocket handshake. It is **not** written to `remotes.json`, not passed on the client argv, and not written to tracing logs. If a failed start's banner is shown, `token=` / `?token=` values are redacted. The tunnel binds `127.0.0.1` only. A missing daemon is copied to `~/.local/bin/fresh-gui` on the remote account you SSH as — same Unix user the daemon will run as.

### 3.6 Test escape hatch

`--allow-no-auth` (also `FRESH_GUI_ALLOW_NO_AUTH`) disables authentication **only on loopback**. Non-loopback + `--allow-no-auth` is a hard error. When enabled, the process logs and prints a clear warning. Integration tests use this flag; it is not part of the normal operator flow.

## 4. Risks and mitigations

| Risk | Notes / mitigation |
|------|---------------------|
| **Token visible via `ps aux` if passed as `--token` on the CLI** | Other users on a shared host can see full command lines of any process. Prefer `FRESH_GUI_TOKEN` or (best) let it auto-generate — neither appears in `ps` output. Documented in README. |
| **Token in shell history** | It's printed once to the terminal and appears in the URL if you pass that URL to `--backend`. Treat it like a password: don't paste it into chat/tickets, and restart the process (new random token) if you suspect it leaked. Prefer `fresh-gui` (local) or `fresh-gui user@host` / `remote connect`, which keep the token off the client argv. |
| **Token in logs/terminal scrollback** | Kept out of `tracing` (which may be centrally aggregated, e.g. journald); it still hits the operator's own terminal scrollback by design (that's the delivery mechanism), so avoid running under a shared/logged terminal multiplexer session. |
| **Token in private `session.json`** | Needed so re-running `fresh-gui` can reprint the Local access URL. Unix: `$XDG_RUNTIME_DIR/fresh-gui/session.json` (mode `0600`, directory `0700`). Windows: `%LOCALAPPDATA%\fresh-gui\session.json`. Removed on `fresh-gui close` / daemon exit. Same user-private trust as the lock file — other accounts cannot read it. |
| **Token comparison timing** | Equal-length compares use a byte-wise XOR fold; length mismatches still short-circuit. In practice the token travels only over loopback or an SSH tunnel, and 122 bits of entropy makes brute forcing infeasible. |
| **SSH tunnel security depends on normal SSH host/key verification** | No new risk introduced — same trust model as any other SSH usage. Verify host keys as usual; don't blindly accept unknown host keys. `remote connect` does not change `StrictHostKeyChecking`. |
| **SSH bootstrap copies a daemon binary onto the remote account** | The bytes come from a path or URL you configured (or the project's GitHub release). They are installed as `~/.local/bin/fresh-gui` for the SSH user and started as that user. The ADE token stays in that user's `session.json` and in the client process; it is not written to `remotes.json`. |
| **Installer downloads release binaries and edits PATH** | `scripts/install.sh` / `scripts/install.ps1` fetch GitHub Release assets for the detected OS, verify the sibling `.sha256` when that asset exists, and install into `~/.fresh-gui/bin` (user PATH / shell rc). A custom `FRESH_GUI_REPOURL` is the download source; credentials embedded in that URL are masked in the script's log lines. Read the script before piping it to a shell. |
| **`--allow-no-auth` misuse** | Restricted to loopback binds only (hard error otherwise) and clearly logged/printed as a warning when used. Intended for local test harnesses only, never documented as a normal run mode. |
| **No rate limiting / lockout on failed auth attempts** | Not added — 122-bit token search space is not meaningfully brute-forceable even without rate limiting. |

## 5. Non-goals

- TLS / `wss://` support, public (non-loopback) exposure by default, or any multi-user account system. The SSH tunnel is the answer to “access from another machine,” not a public bind.
- Persisting tokens across daemon *restarts* — a fresh random token per process start is intentional. Within one background session the token is kept in private `session.json` only so status can reprint it; it is not reused after `fresh-gui close`.
