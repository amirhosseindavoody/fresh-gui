# fresh-gui Design

Terminal-first IDE GUI on the **local host**, powered by a **remote Fresh editor backend**. Product inspiration: [Terax](https://github.com/crynta/terax-ai). Backend performance and semantics: [Fresh](https://github.com/sinelaw/fresh) crates (local clone: `/home/amirhossein/fresh`). Repo logistics mirror [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise): Pixi-managed Rust workspace, `docs/` for design, phased implementation.

## 1. Problem

Developers often keep a Windows (or macOS) laptop as the interactive machine and a Linux box (or WSL, cloud VM) as the real workspace. Existing options either:

- run a full IDE remotely and stream pixels (latency, heavy chrome), or
- put a thin browser UI on the same machine as the editor (`fresh --web`), or
- ship a native GUI in-process with the editor (`fresh` + `fresh-gui` / winit).

**fresh-gui** targets a different split: a **local, terminal-first ADE shell** that connects to a **Fresh session daemon on the remote machine**. The remote owns files, PTY, LSP, and editor state; the host owns windowing, layout, and input devices.

## 2. Goals and Non-Goals

### Goals (MVP)

- **Split deployment:** Windows host GUI ↔ Linux remote backend.
- **Terminal-first UX:** multi-tab / split PTY as the primary surface (Terax-like), with editor/explorer as peers—not an afterthought.
- **Fresh as backend of truth:** reuse Fresh crates / session model; do not re-implement buffer, PTY, or LSP logic in the GUI.
- **Pixi + Cargo workspace** for reproducible Rust development (pixi-mise-shaped layout).
- **Documented protocol** between host and remote with versioned messages.
- **Secure-by-default remote access** (auth + encrypted transport) before any public exposure.

### Non-Goals (MVP)

- Feature parity with Terax AI agents, themes marketplace, or web preview.
- Replacing Fresh’s TUI or shipping as a fork of Fresh itself.
- macOS / Linux host GUI (post-MVP; architecture should not block them).
- Multi-user collaborative editing.
- Packaging to winget / conda-forge on day one (scaffold may reserve `recipe/` later).

## 3. Prior Art

### 3.1 Terax

[Terax](https://github.com/crynta/terax-ai): Tauri 2 + Rust + React, ~7–8 MB, terminal-first ADE with WebGL xterm, CodeMirror, source control, AI side panel. **Local monolith**—PTY and UI share one process. We borrow layout and “terminal is the hero” product sense, not the in-process architecture.

### 3.2 Fresh

Fresh is a high-performance terminal editor (Rust) with:

| Surface | Role |
|---------|------|
| TUI (`crossterm`) | Primary product |
| `crates/fresh-gui` | In-process winit + wgpu ratatui window |
| `fresh --web` + `web-ui/` | Browser chrome over the **same** `Editor` via WebSocket scene push |

Orchestrator already models **SSH / remote workspaces** (sessions live on remote authorities). That is complementary: Fresh can already *be* remote-aware internally; this project adds a **host-native ADE GUI** that treats Fresh as an explicit remote service.

Relevant Fresh docs (in the Fresh tree): `docs/architecture.md`, `docs/internal/web-ui.md`, orchestrator / per-session backend designs.

### 3.3 pixi-mise

Logistics template: `pixi.toml` + Cargo workspace under `crates/`, `docs/DESIGN.md`, CalVer `YYYY.MMDD.N`, `scripts/update-version.sh`, optional `recipe/` later.

## 4. Architecture Overview

```
┌─────────────────────────────┐         ┌──────────────────────────────────┐
│  Host (Windows MVP)         │         │  Remote (Linux MVP)              │
│                             │  TLS /  │                                  │
│  fresh-gui-app              │  SSH    │  fresh-gui-backend               │
│    layout, tabs, chrome     │◄───────►│    wraps Fresh session / Editor  │
│    terminal renderer        │  proto  │    PTY, files, LSP, buffers      │
│                             │         │                                  │
│  fresh-gui-client           │         │  fresh-editor / fresh-core / …   │
│    connect, auth, framing   │         │  (path or git dep on Fresh)      │
└─────────────────────────────┘         └──────────────────────────────────┘
                 ▲                                        ▲
                 │                                        │
                 └──────── fresh-gui-protocol ────────────┘
                           (shared message types)
```

### 4.1 Crates

| Crate | Binary? | Platform (MVP) | Responsibility |
|-------|---------|----------------|----------------|
| `fresh-gui-protocol` | no | all | Versioned messages, capability negotiation, errors |
| `fresh-gui-backend` | yes (`fresh-gui-backend`) | linux | Daemon: bind transport, host Fresh session(s), serve protocol |
| `fresh-gui-client` | no | all (host) | Dial, auth, reconnect, typed RPC / streams |
| `fresh-gui-app` | yes (`fresh-gui`) | windows | Tauri 2 shell + web UI (xterm.js); dials backend via `fresh-gui-client` |

Dev machines (e.g. WSL) may build backend + protocol + client locally; the Windows GUI is cross-built or built on Windows CI / a Windows host.

### 4.2 Process model

1. Operator starts **`fresh-gui-backend`** on the Linux remote (systemd user unit, `pixi run`, or SSH remote-command).
2. Operator starts **`fresh-gui`** on Windows, enters host:port (or SSH jump), authenticates.
3. GUI opens one or more **sessions** (working directory / project). Backend creates or attaches a Fresh editor session.
4. Terminal panes map to remote PTYs managed by Fresh (or a thin PTY bridge if MVP scopes editor later—see open decisions).
5. Disconnect leaves the remote session running when the backend supports detach (align with Fresh session daemon semantics where possible).

## 5. Protocol (sketch)

**Resolved (D1):** new ADE protocol, **PTY-first**. Do **not** reuse Fresh `--web` scene as the primary wire. Fresh Editor / scene may be added later as optional capabilities (Phase 3+), not as the MVP transport.

The protocol crate owns:

- **Hello / version** — `protocol_version`, backend implementation id, advertised capabilities (e.g. `pty`, later `scene` / `fs`).
- **Auth** — token / mTLS / SSH-channel; reject anonymous binds on non-loopback.
- **Session** — create / attach / list / dispose.
- **Streams (MVP / Phase 1)** — terminal I/O (bytes + resize) under a `pty` capability.
- **Streams (Phase 1b)** — read-only remote file tree under an `fs` capability (list / stat); negotiated, not required for Phase 1 connect.
- **Streams (Phase 3+)** — optional Fresh scene diffs, diagnostics — negotiated via capabilities.
- **Control** — ping/pong, graceful shutdown, error envelopes.

Wire framing (JSON over WebSocket vs length-prefixed MessagePack/CBOR, etc.) can be chosen during Phase 1 implementation; it must not imply Fresh web-ui message shapes.

## 6. MVP Scope (phased)

### Phase 0 — Scaffold (this commit)

- `docs/DESIGN.md`, Pixi + Cargo workspace, crate stubs, README.
- No Fresh linkage yet; protocol types are placeholders.

### Phase 1 — Remote PTY loop (D4)

- Backend exposes authenticated PTY create/read/write/resize.
- Host GUI (Tauri 2 + xterm.js): connect dialog + single terminal tab.
- Prove latency and reconnect on WSL ↔ Windows and real SSH.
- **No** file tree / editor in this phase.

### Phase 1b — Read-only file tree (immediately after Phase 1)

- Protocol `fs` capability: list directory, open path as context (read-only tree in the GUI).
- Ships right after the PTY loop works—before multi-tab polish if needed.

### Phase 2 — Multi-tab / splits + session detach

- Multiple PTYs, horizontal/vertical splits, session survives GUI restart.

### Phase 3 — Editor surface (Fresh pull-in)

- Extend the ADE protocol with optional capabilities that expose Fresh Editor / scene (or buffer APIs), without replacing the PTY-first path.
- File tree from Phase 1b deepens (open-in-editor, watch) as needed.

### Phase 4 — Polish / packaging

- Windows installer, Linux backend package, auto-update story, hardening.

## 7. Development Environment

### Pixi

- Channels: conda-forge.
- Platforms: at least `linux-64` for backend/dev on WSL; `win-64` added when GUI builds are routine.
- Tasks: `check`, `test`, `build`, `clippy`, `fmt`, `update-version`.
- Rust toolchain via Pixi `rust` dependency (same idea as pixi-mise).

### Fresh dependency

**Resolved (D3):** git submodule at `vendor/fresh`, pinned by commit SHA. Backend Cargo deps use `path = "../../vendor/fresh/crates/…"`. No Fresh linkage in Phase 0 stubs.

### Versioning

CalVer compatible with Cargo: `YYYY.MMDD.N` (e.g. `2026.728.1`). `scripts/update-version.sh` bumps workspace manifests.

## 8. Security (baseline)

- No unauthenticated listen on `0.0.0.0`.
- Prefer SSH port-forward or SSH subsystem / unix-socket + local proxy for early MVP over raw public TCP.
- Tokens in OS keychain on the host; never in repo.
- Capability negotiation so older GUIs fail closed against newer backends when incompatible.

## 9. License

**Resolved (D5):** the entire `fresh-gui` project (all crates) is **GPL-2.0**, matching [Fresh](https://github.com/sinelaw/fresh). The `vendor/fresh` submodule remains under Fresh’s own GPL-2.0 terms.

## 10. Decisions

### Resolved

#### D1 — Transport / protocol base — **B**

**Decision:** New ADE protocol (PTY-first). Pull Fresh Editor/scene in later as optional capabilities (Phase 3+).

| Option | Pros | Cons |
|--------|------|------|
| A. Reuse Fresh `fresh --web` WebSocket + scene | Real Editor already; less new protocol | Scene is editor-centric; terminal-first ADE may fight the grid model |
| **B. New ADE protocol (PTY-first) + optional scene later** ✓ | Clean MVP (Phase 1 PTY); Terax-like UX freedom | Duplicate some session concepts; more design work |
| C. SSH + local Fresh only (no custom daemon) | Minimal new code | Weak split-host product |

#### D2 — Host GUI stack — **A**

**Decision:** Tauri 2 + web frontend with **xterm.js** (WebGL renderer), Terax-like. Rust side owns connection/`fresh-gui-client`; UI owns layout and terminal panes.

| Option | Pros | Cons |
|--------|------|------|
| **A. Tauri 2 + web frontend** (xterm.js) ✓ | Fast UI iteration, WebGL xterm, large ecosystem | Web stack + Rust bridge; package size vs egui |
| B. egui / iced / GPUI | Pure Rust | Weaker terminal emulator ecosystem |
| C. Native webview wrapping Fresh web-ui | Reuses Fresh UI | Not terminal-first ADE; scene-centric |

#### D3 — Fresh coupling — **submodule + git rev pin**

**Decision:** Vendor Fresh as a **git submodule** at `vendor/fresh`, pinned to an explicit **git revision** (not a floating branch). Cargo path dependencies point into `vendor/fresh/crates/…` when the backend links Fresh (Phase 3+; optional earlier for PTY helpers).

| Option | Pros | Cons |
|--------|------|------|
| A. Path dependency on sibling checkout | Fast local iteration | Not portable |
| B. Cargo git dependency only | Reproducible in Cargo.lock | No in-tree sources; awkward patches |
| **C. Submodule + pin by git rev** ✓ | Explicit pin, portable clone, editable tree | Extra `submodule update` step |
| D. crates.io only | Clean | APIs may not ship for this use |

**Current pin (Phase 0):** `f5f2c4639f7d5ed3d6b3ef3d2343365ced426401` (`Merge upstream/master into master` on the integration fork).

**Remote:** prefer the fork used for integration work (`https://github.com/amirhosseindavoody/fresh.git`); bump the pin with `git -C vendor/fresh fetch && git -C vendor/fresh checkout --detach <rev>` then stage `vendor/fresh` in this repo. Upstream tracking: `https://github.com/sinelaw/fresh.git`.

**Clone:**

```bash
git clone --recurse-submodules https://github.com/amirhosseindavoody/fresh-gui.git
# or after clone:
git submodule update --init --recursive
```

#### D4 — MVP feature cut — **PTY now, file tree next**

**Decision:** Phase 1 is **PTY-only**. A **read-only remote file tree** follows immediately as **Phase 1b** (not deferred to editor work). Full Fresh scene stays Phase 3.

| Option | Pros | Cons |
|--------|------|------|
| **A. PTY-only (Phase 1)** ✓ | Ships a useful remote terminal ADE shell quickly | No explorer yet |
| **B. File tree next (Phase 1b)** ✓ | IDE-like chrome soon after PTY | Extra protocol surface, sequenced deliberately |
| C. Full Fresh scene from day one | Maximum Fresh leverage | Delays terminal-first polish |

#### D5 — License — **GPL-2.0 (entire project)**

**Decision:** License all `fresh-gui` crates under **GPL-2.0** (same as Fresh). No split licensing.

| Option | Pros | Cons |
|--------|------|------|
| MIT / Apache host + GPL backend | Permissive reuse of protocol/GUI | Two regimes to maintain |
| **GPL-2.0 everywhere** ✓ | Simple; always safe when linking Fresh | Copyleft on host/protocol too |

All decisions D1–D5 are resolved.

---

## 11. Repository Layout

```
fresh-gui/
  Cargo.toml              # workspace
  Cargo.lock
  pixi.toml / pixi.lock
  README.md
  LICENSE
  .gitignore
  docs/
    DESIGN.md             # this file
  vendor/
    fresh/                # git submodule (pinned rev; see D3)
  crates/
    fresh-gui-protocol/
    fresh-gui-backend/
    fresh-gui-client/
    fresh-gui-app/          # Tauri 2 host (src-tauri + web UI; lands in Phase 1)
  scripts/
    update-version.sh
```

## 12. Success Criteria (MVP)

- From Windows GUI: connect to Linux backend, open a shell, type with acceptable latency on LAN / SSH.
- Backend process can run detached; reconnect restores the same PTY session(s).
- `pixi run check` and `pixi run test` pass on the Linux/WSL dev machine.
- Decisions D1–D5 resolved (PTY-first ADE; Tauri 2 + xterm.js; Fresh submodule + rev pin; Phase 1 PTY then 1b file tree; GPL-2.0 everywhere).
