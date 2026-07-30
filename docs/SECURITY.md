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
   - the exact **SSH tunnel command** to run on a laptop plus the URL to open in the laptop's browser afterward.
4. Keep a narrow, explicit escape hatch for local integration tests (`--allow-no-auth`, loopback-only) so CI doesn't need to thread tokens through every test — never used in the documented user-facing flow.

## 3. Design

### 3.1 Token lifecycle

- If `--token` / `FRESH_GUI_TOKEN` is set (and non-empty), use it as-is (lets an operator pin a stable token, e.g. to reuse across restarts).
- Otherwise, **auto-generate** a random token per process start (`uuid::Uuid::new_v4()`, 122 bits of OS-RNG entropy, formatted as 32 hex chars). It is never written to git/config.
- The token is **never included in `tracing` structured logs** (only `auth_required: bool` is logged). For the background session, it is stored in the user-private session meta file (`$XDG_RUNTIME_DIR/fresh-gui/session.json`, mode `0600`) so a later `fresh-gui` / `fresh-gui status` can reprint the Local access URL. The meta file is removed on `fresh-gui close`.
- Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in `ps` / process listings.

### 3.2 Bind stays loopback by default

`--listen` defaults to `127.0.0.1:7420`. Non-loopback binds remain possible for advanced setups but are not the documented path; a `warn!` log fires when the bind is non-loopback, nudging the operator back to loopback + SSH tunnel.

### 3.3 Background session + startup banner

Default `fresh-gui` detaches a per-user daemon (exclusive flock), prints status, and returns the shell. Re-running `fresh-gui` reprints status; `fresh-gui close` sends SIGTERM. Daemon stdout/stderr go to `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` (fallback `~/.local/state/fresh-gui/`).

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
    then open: http://127.0.0.1:7420/?token=<token>

  Stop with: fresh-gui close
```

`<user>` comes from `$USER` / `$LOGNAME`; `<host>` reuses the existing FQDN-detection helper (`assigned_host_domain()`) already used for the plain `UI:` / `WS:` lines, falling back to a placeholder if no domain is detectable. The port in the local / tunnel lines follows the **bound** port (after any listen fallback).

### 3.4 Frontend convenience

The UI's `?token=` query param (if present) is read on load and used to auto-connect — pasting either printed URL into a browser “just works”. The query param is stripped from the visible address bar via `history.replaceState` right after it's read, so it doesn't linger in browser history / autocomplete. There is no connect form; connection status lives in the status bar, and Disconnect / Reconnect are command-palette actions.

After a successful read (and after `auth_ok`), the token is also kept in **`sessionStorage`** (`fresh-gui.authToken`) for this browser tab. Reloading the page reuses that cache plus the existing `localStorage` session id so the client can auth and `session_attach` again without the token remaining in the URL. The cache is cleared on `auth_error` and when the tab is closed (`sessionStorage` lifetime) — it is not written to disk by the backend and is not shared across tabs.

### 3.5 Test escape hatch

`--allow-no-auth` (also `FRESH_GUI_ALLOW_NO_AUTH`) disables authentication **only on loopback**. Non-loopback + `--allow-no-auth` is a hard error. When enabled, the process logs and prints a clear warning. Integration tests use this flag; it is not part of the normal operator flow.

## 4. Risks and mitigations

| Risk | Notes / mitigation |
|------|---------------------|
| **Token visible via `ps aux` if passed as `--token` on the CLI** | Other users on a shared host can see full command lines of any process. Prefer `FRESH_GUI_TOKEN` or (best) let it auto-generate — neither appears in `ps` output. Documented in README. |
| **Token in shell/browser history** | It's printed once to the terminal and appears in the URL if you paste the printed link. Treat it like a password: don't paste it into chat/tickets, and restart the process (new random token) if you suspect it leaked. The frontend strips it from the address bar after first use to reduce lingering exposure. |
| **Token in `sessionStorage` for reload reconnect** | Needed so reload can auth after `?token=` is stripped. Scoped to the tab (cleared when the tab closes), wiped on `auth_error`, and never logged. Accessible to page script (same XSS class as any in-memory secret). |
| **Token in logs/terminal scrollback** | Kept out of `tracing` (which may be centrally aggregated, e.g. journald); it still hits the operator's own terminal scrollback by design (that's the delivery mechanism), so avoid running under a shared/logged terminal multiplexer session. |
| **Token in `$XDG_RUNTIME_DIR/fresh-gui/session.json`** | Needed so re-running `fresh-gui` can reprint the Local access URL. File mode `0600`, directory `0700`, removed on `fresh-gui close` / daemon exit. Same user-private trust as the lock file — other accounts cannot read it. |
| **Token comparison timing** | Equal-length compares use a byte-wise XOR fold; length mismatches still short-circuit. In practice the token travels only over loopback or an SSH tunnel, and 122 bits of entropy makes brute forcing infeasible. |
| **SSH tunnel security depends on normal SSH host/key verification** | No new risk introduced — same trust model as any other SSH usage. Verify host keys as usual; don't blindly accept unknown host keys. |
| **`--allow-no-auth` misuse** | Restricted to loopback binds only (hard error otherwise) and clearly logged/printed as a warning when used. Intended for local test harnesses only, never documented as a normal run mode. |
| **No rate limiting / lockout on failed auth attempts** | Not added — 122-bit token search space is not meaningfully brute-forceable even without rate limiting. |

## 5. Non-goals

- TLS / `wss://` support, public (non-loopback) exposure by default, or any multi-user account system. The SSH tunnel is the answer to “access from another machine,” not a public bind.
- Persisting tokens across daemon *restarts* — a fresh random token per process start is intentional. Within one background session the token is kept in private `session.json` only so status can reprint it; it is not reused after `fresh-gui close`.
