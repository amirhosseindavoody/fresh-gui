# Secure Access Design: Always-On Token + SSH Tunnel

Status: **implemented** (see also [DESIGN.md §8](./DESIGN.md#8-security-baseline)).

This document is the security design for how operators reach `fresh-gui-backend`
on shared hosts. User-facing install steps: [README.md](../README.md).

## 1. Problem

`fresh-gui-backend` typically runs on a **shared multi-user Linux server** (a compute
node, dev box, etc.), not a personal single-user machine. It exposes a real PTY
(interactive shell) and file system access under the operator's own Unix
permissions to whoever can complete the ADE WebSocket handshake.

Before always-on auth:

- A loopback bind (`127.0.0.1:7420`, the default) required **no auth token at
  all** unless the operator explicitly passed `--token`.
- `127.0.0.1` is a property of the machine's network stack, **not** of a Unix
  user — every other user logged into the same shared host can already open a
  TCP connection to `127.0.0.1:7420`. An unauthenticated loopback bind is
  therefore reachable by any other local account on that host, not just the
  person who started the process.
- There was no convenient, copy-pasteable way to reach the backend from a
  laptop; the operator had to hand-build an SSH tunnel command and remember to
  pass a token.

## 2. Requirements

1. **Always require a bearer token** to authenticate a session, even on the
   default loopback bind. No opt-out in normal operation.
2. **Never expose the backend to the network by default.** Keep the loopback
   bind as the default and the recommended path — remote access goes through
   an **SSH tunnel**, which is already authenticated (SSH login) and encrypted
   end to end. No TLS / public-bind story is introduced here.
3. **Zero-friction access instructions.** On startup, the process prints:
   - a **local** URL (same machine) with the token embedded, and
   - the exact **SSH tunnel command** to run on a laptop plus the URL to open
     in the laptop's browser afterward.
4. Keep a narrow, explicit escape hatch for local integration tests
   (`--allow-no-auth`, loopback-only) so CI doesn't need to thread tokens
   through every test — never used in the documented user-facing flow.

## 3. Design

### 3.1 Token lifecycle

- If `--token` / `FRESH_GUI_TOKEN` is set (and non-empty), use it as-is
  (lets an operator pin a stable token, e.g. to reuse across restarts).
- Otherwise, **auto-generate** a random token per process start
  (`uuid::Uuid::new_v4()`, 122 bits of OS-RNG entropy, formatted as 32 hex
  chars). It is never written to disk or config — matches the baseline
  (“tokens never in repo”).
- The token is **never included in `tracing` structured logs** (only
  `auth_required: bool` is logged). It is only ever written via plain
  `println!` in the startup banner, once, to the operator's own terminal.
- Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in
  `ps` / process listings.

### 3.2 Bind stays loopback by default

`--listen` defaults to `127.0.0.1:7420`. Non-loopback binds remain possible for
advanced setups but are not the documented path; a `warn!` log fires when the
bind is non-loopback, nudging the operator back to loopback + SSH tunnel.

### 3.3 Startup banner

```text
  fresh-gui ready
  UI:  http://127.0.0.1:7420/
  WS:  ws://127.0.0.1:7420/ws

  Local access (this machine):
    http://127.0.0.1:7420/?token=<token>

  From another machine (e.g. your laptop) — SSH tunnel, nothing exposed to the network:
    ssh -L 7420:127.0.0.1:7420 <user>@<host>
    then open: http://127.0.0.1:7420/?token=<token>
```

`<user>` comes from `$USER` / `$LOGNAME`; `<host>` reuses the existing
FQDN-detection helper (`assigned_host_domain()`) already used for the plain
`UI:` / `WS:` lines, falling back to a placeholder if no domain is detectable.
The port in the local / tunnel lines follows the **bound** port (after any
listen fallback).

### 3.4 Frontend convenience

The UI's `?token=` query param (if present) pre-fills the **Token** field and
auto-connects, so pasting either printed URL into a browser “just works”
without manually copying the token into the connect form. The query param is
stripped from the visible address bar via `history.replaceState` right after
it's read, so it doesn't linger in browser history / autocomplete.

### 3.5 Test escape hatch

`--allow-no-auth` (also `FRESH_GUI_ALLOW_NO_AUTH`) disables authentication
**only on loopback**. Non-loopback + `--allow-no-auth` is a hard error. When
enabled, the process logs and prints a clear warning. Integration tests use
this flag; it is not part of the normal operator flow.

## 4. Risks and mitigations

| Risk | Notes / mitigation |
|------|---------------------|
| **Token visible via `ps aux` if passed as `--token` on the CLI** | Other users on a shared host can see full command lines of any process. Prefer `FRESH_GUI_TOKEN` or (best) let it auto-generate — neither appears in `ps` output. Documented in README. |
| **Token in shell/browser history** | It's printed once to the terminal and appears in the URL if you paste the printed link. Treat it like a password: don't paste it into chat/tickets, and restart the process (new random token) if you suspect it leaked. The frontend strips it from the address bar after first use to reduce lingering exposure. |
| **Token in logs/terminal scrollback** | Kept out of `tracing` (which may be centrally aggregated, e.g. journald); it still hits the operator's own terminal scrollback by design (that's the delivery mechanism), so avoid running under a shared/logged terminal multiplexer session. |
| **Token comparison timing** | Equal-length compares use a byte-wise XOR fold; length mismatches still short-circuit. In practice the token travels only over loopback or an SSH tunnel, and 122 bits of entropy makes brute forcing infeasible. |
| **SSH tunnel security depends on normal SSH host/key verification** | No new risk introduced — same trust model as any other SSH usage. Verify host keys as usual; don't blindly accept unknown host keys. |
| **`--allow-no-auth` misuse** | Restricted to loopback binds only (hard error otherwise) and clearly logged/printed as a warning when used. Intended for local test harnesses only, never documented as a normal run mode. |
| **No rate limiting / lockout on failed auth attempts** | Not added — 122-bit token search space is not meaningfully brute-forceable even without rate limiting. Could be added later if the threat model changes (e.g. if non-loopback binds become common). |

## 5. Non-goals

- TLS / `wss://` support, public (non-loopback) exposure by default, or any
  multi-user account system. The SSH tunnel is the answer to “access from
  another machine,” not a public bind.
- Persisting or rotating tokens across restarts — a fresh random token per
  process start is intentional and simpler to reason about.
