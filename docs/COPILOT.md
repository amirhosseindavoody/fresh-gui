# Copilot CLI integration design

Design record for [issue #49](https://github.com/amirhosseindavoody/fresh-gui/issues/49): use GitHub Copilot CLI with fresh-gui for autocomplete and/or agentic development, without the VS Code Copilot extension’s UI cost.

**Status:** design only — no implementation in this revision.  
**Related:** [DESIGN.md](./DESIGN.md), [FRESH.md](./FRESH.md), [UI.md](./UI.md). Fresh prior art: `vendor/fresh` Orchestrator + [agent CLI exposure plan](../vendor/fresh/docs/internal/agent-fresh-cli-exposure-plan.md).

---

## 1. Problem

The VS Code Copilot extension can make the editor UI feel sluggish. The ask is whether **Copilot CLI** can give fresh-gui users:

1. **Autocomplete** (inline / ghost-text style completions while typing), and/or  
2. **Agentic development** (plan, edit files, run tools, iterate on a task).

fresh-gui already provides a remote Linux workspace with real PTYs and a Fresh-backed editor. The opportunity is to keep AI work on the remote machine (where the code and shell live) and keep the local host chrome light.

## 2. Investigation summary

### 2.1 What Copilot CLI is

GitHub Copilot CLI (`@github/copilot`, GA as of early 2026) is a **terminal-native agentic coding agent**, not an editor completion engine. It:

- Runs interactively (`copilot`) or programmatically (`copilot -p "…"`).
- Plans, edits files, runs shell tools (with approval), talks to GitHub (built-in MCP), supports Autopilot, `/plan`, custom agents, skills, hooks, and session resume (`--continue` / `--resume`).
- Speaks the open **Agent Client Protocol (ACP)** via `copilot --acp` (stdio or TCP) so IDEs can be ACP *clients* without embedding the VS Code extension.

It does **not** expose LSP-style or ghost-text autocomplete APIs for arbitrary editors. Slash-command “autocomplete” inside the CLI TUI is unrelated to CodeMirror inline completions.

### 2.2 What fresh-gui has today

| Area | State |
|------|--------|
| Terminal | Real remote PTY (`portable-pty` + xterm). Any CLI, including `copilot`, runs today if installed on the remote. |
| Agent UX | None. No Run Agent, resume metadata, activity indicator, or chat rail. |
| Editor | CodeMirror 6; syntax highlight only. No `@codemirror/autocomplete`, no LSP over ADE. |
| Fresh embedding | Buffer open/edit/save only (`runtime`). Plugins / Orchestrator **off**. Fresh’s own TerminalManager unused. |
| Protocol | ADE `0.4.0`: `ping`, `pty`, `session`, `fs`, `editor`, `scene`. No agent / ACP / completion family. |
| Right rail | Reserved (width 0) in [UI.md](./UI.md); AI chat rail was an explicit non-goal vs Terax. |

**Works with zero code:** open a terminal tab, `npm i -g @github/copilot`, authenticate (`/login`), run `copilot` in the workspace root. Disconnect/reattach keeps the PTY alive (session scrollback ~64 KiB). Closing the pane kills the process.

### 2.3 What Fresh already solved (not yet bridged)

Fresh’s Orchestrator plugin treats coding agents as first-class terminal sessions:

- Registry: Claude Code, Codex, OpenCode, Aider (+ custom).
- Launch with initial prompt / auto-permission flags; resume via agent-specific argv.
- Coarse activity from terminal output; worktree-per-task workspaces.
- Optional `FRESH_SESSION` / `FRESH_CMD_TOKEN` so agents can drive the editor via `fresh --cmd script run`.

There is **no Copilot entry** in Fresh’s registry today, but the pattern maps cleanly (`copilot`, `copilot --continue`, Autopilot ≈ `--allow-all` / mode cycling).

fresh-gui cannot reuse that plugin until plugins are enabled or the same lifecycle is reimplemented against ADE PTYs (see workspace rule: prefer Fresh surfaces first).

### 2.4 Autocomplete vs agents (split the problem)

| Need | Copilot CLI fit | Better path in fresh-gui |
|------|-----------------|---------------------------|
| Agentic edit / plan / run | **Strong** — interactive PTY or ACP client | PTY-first, then ACP |
| Inline autocomplete while typing | **Poor** — not the CLI’s product surface | Fresh LSP + completion framework over ADE, or a separate completions product if GitHub exposes one |
| “Don’t slow down the UI” | Strong for agents (out-of-process CLI / ACP) | Avoid embedding VS Code’s Copilot extension; keep heavy work on remote |

**Conclusion:** treat issue #49 primarily as **agent integration**. Treat autocomplete as a **separate language-intelligence track** (Fresh LSP), not as a Copilot CLI feature.

## 3. Goals and non-goals

### Goals

- Let a Copilot subscriber run agentic workflows against the remote workspace without VS Code.
- Prefer **terminal-native** UX first (matches fresh-gui’s terminal-first product).
- Keep the host UI light: agent process lives on the **remote** daemon machine.
- Design for a path that can later generalize to Claude / Codex / etc. (Fresh Orchestrator shape), not a Copilot-only one-off if avoidable.
- Document how ACP fits if/when a structured chat surface is wanted.

### Non-goals (this design)

- Feature parity with VS Code Copilot Chat, inline ghost text, or Copilot Nes.
- Shipping a hosted LLM or bundling Copilot credentials into the daemon binary.
- Replacing Fresh’s Orchestrator inside Fresh’s TUI; this is about the **ADE host**.
- Full Terax-style AI chrome (composer + agent diffs marketplace) as a near-term requirement.
- Implementing autocomplete via Copilot CLI.

## 4. Options

### Option A — Document “just run it in a terminal” (no product surface)

**What:** README / docs tip: install Copilot CLI on the remote, run `copilot` in a fresh-gui PTY.

| Pros | Cons |
|------|------|
| Zero engineering; already works | No discoverability, resume chrome, or editor awareness |
| Avoids VS Code entirely | Pane close kills agent; limited scrollback on reattach |
| Matches terminal-first ethos | No answer for “autocomplete” |

**Verdict:** ship as **Phase 0** (docs only) immediately when implementing starts.

### Option B — Thin “Run Copilot” launcher (PTY preset)

**What:** Command palette / activity entry that opens (or focuses) a terminal pane and starts `copilot` with cwd = workspace / active terminal cwd. Optional: initial prompt, `--continue`, Autopilot flags.

Mirrors Fresh Orchestrator’s agent registry **without** enabling Fresh plugins: registry lives in fresh-gui (or a thin ADE `agent_*` API that spawns PTYs with known argv).

| Pros | Cons |
|------|------|
| Small surface; reuses existing PTY stack | Still a TUI inside xterm, not a native chat panel |
| Generalizes to other CLIs | Resume/activity heuristics are agent-specific |
| Aligns with Fresh’s proven agent model | Does not teach the agent to drive ADE editor chrome |

**Verdict:** **recommended first product increment** after Phase 0.

### Option C — ACP client in fresh-gui (structured agent UI)

**What:** Daemon or host becomes an ACP client. Spawn `copilot --acp --stdio` on the remote (or connect TCP loopback). Host right rail (or modal) shows streaming messages, plans, tool calls; user answers `requestPermission`; optional client `fs/*` and `terminal/*` capabilities map to ADE FS / PTY.

```
┌─ Host UI (browser) ─┐     ADE /ws      ┌─ fresh-gui daemon ─┐     stdio      ┌─ copilot --acp ─┐
│ Agent panel / rail  │◄───────────────►│ ACP bridge         │◄─────────────►│ Copilot agent   │
│ Permission prompts  │   agent_* msgs  │ (remote process)   │   NDJSON      │ (subscriber)    │
│ Editor refreshes    │                 │ FS/PTY adapters    │               └─────────────────┘
└─────────────────────┘                 └────────────────────┘
```

| Pros | Cons |
|------|------|
| Proper IDE integration; no VS Code extension | New protocol family + UI; ACP still public preview |
| Streaming, permissions, slash commands over ACP | Must decide daemon-vs-host ACP ownership |
| Same ACP surface could later host other agents | File edits outside Fresh buffers need reload / watch |
| Matches GitHub’s recommended IDE path | Risk of rebuilding Terax-like chrome we deferred |

**Verdict:** **Phase 2** once Phase 1 launcher proves demand. Prefer ACP over inventing a proprietary chat protocol.

### Option D — Enable Fresh plugins + Orchestrator in the daemon

**What:** Turn on Fresh `plugins` / `embed-plugins`, load Orchestrator, add a `copilot` registry entry, bridge workspaces somehow into ADE tabs.

| Pros | Cons |
|------|------|
| Reuses largest existing agent codebase | Fresh plugins assume Fresh’s own terminal/window model |
| Script channel (`FRESH_CMD_TOKEN`) for editor drive | ADE uses separate PTY + CodeMirror; deep mismatch |
| | Large embedding change; conflicts with current narrow Fresh surface |

**Verdict:** **not the primary path** for fresh-gui. Steal patterns (registry, resume argv, system-prompt injection); do not load the full Orchestrator plugin into the ADE daemon until Fresh’s terminal and ADE PTY are unified (unlikely near-term).

### Option E — Inline autocomplete via Copilot

**What:** Call some Copilot completion API from CodeMirror on each keystroke.

| Pros | Cons |
|------|------|
| Answers the autocomplete half of #49 literally | Copilot CLI does not provide this; VS Code path is the heavy extension |
| | Would reintroduce the latency/UI cost the issue wants to escape |
| | Separate auth, product, and protocol work |

**Verdict:** **out of scope** for Copilot CLI integration. If autocomplete is needed, design a Fresh-LSP ADE capability separately.

## 5. Recommended approach

Phased, terminal-first, Copilot as the first agent preset:

| Phase | Deliverable | Protocol / code impact |
|-------|-------------|------------------------|
| **0** | Docs: install, auth, run `copilot` in a fresh-gui terminal; limitations (kill on pane close, scrollback) | Docs only |
| **1** | “Run Copilot” (and later “Run agent…”) palette action; PTY spawn with cwd + optional prompt / continue / yolo | Optional thin `agent` config; no new wire messages required if host opens PTY with `shell` argv |
| **2** | ACP bridge + agent panel (right rail): stream, permissions, cancel; fs watch → refresh open editors | New ADE capability `agent` (or `acp`); daemon owns `copilot --acp` child |
| **3** | Multi-agent registry (Claude, Codex, …), resume UX, optional “open files the agent touched” | Config + UI; still no Fresh Orchestrator plugin load |
| **Later / parallel** | Language intelligence (Fresh LSP → ADE → CodeMirror) for real autocomplete | Separate design; not Copilot CLI |

### Why this order

1. Phase 0 unblocks the issue author immediately.  
2. Phase 1 matches Fresh’s proven “agent = terminal session” model and stays inside existing ADE capabilities (`pty`).  
3. Phase 2 uses GitHub’s intended IDE seam (ACP) instead of scraping the Copilot TUI.  
4. Autocomplete is explicitly deferred so the design does not pretend CLI ≈ Nes/ghost text.

## 6. Phase 1 design (launcher)

### 6.1 UX

- Command palette: **Run Copilot**, **Continue Copilot** (`copilot --continue`).
- Optional follow-ups: prompt dialog → `copilot -p "…"` or interactive with seed (prefer interactive TUI for day-to-day).
- Opens a **new terminal tab** (or split) with cwd = active terminal OSC 7 cwd, else FS root.
- Status bar / tab label: `copilot` while the process is the pane’s child (best-effort from argv).

No right-rail chat in Phase 1.

### 6.2 Process model

Reuse `pty_open` with non-empty `shell.command` / `args` (existing behavior: when args are non-empty, OSC 7 wrappers are skipped — acceptable for a dedicated agent pane).

Example spawn:

```text
command: copilot
args: []                    # interactive
# or args: ["--continue"]
# or args: ["-p", "<prompt>", "--allow-all"]   # use sparingly; document risk
```

Require `copilot` on `PATH` on the **remote**. Surface a clear error if spawn fails (`ENOENT`).

### 6.3 Config sketch (optional)

```jsonc
// ~/.config/fresh-gui/config.json
"agents": {
  "copilot": {
    "command": "copilot",
    "args": [],
    "continueArgs": ["--continue"]
  }
}
```

Shape intentionally close to Fresh Orchestrator registry entries so Phase 3 can add siblings without a schema break.

### 6.4 Editor / FS interaction

Copilot edits files on disk. Phase 1 relies on existing `fs_watch` + user re-open, or a small host improvement: if an open editor’s path changes on disk, prompt reload (nice-to-have, not blocking).

Do **not** invent agent→editor script control in Phase 1 (Fresh’s `FRESH_CMD_TOKEN` is unavailable without plugins / control socket).

### 6.5 Fresh check (rule compliance)

| Checked in Fresh | Outcome |
|------------------|---------|
| Orchestrator agent registry + resume | Pattern to copy; plugin not loadable in current ADE embed |
| Agent script / `fresh --cmd` channel | Not available without plugins + Fresh terminal inheritance |
| Copilot-specific support | None in vendor Fresh |

**Invented in fresh-gui:** only the ADE-side launcher wiring to existing PTYs — because Fresh’s agent stack is plugin/TUI-bound.

## 7. Phase 2 design (ACP)

### 7.1 Ownership

**ACP child process runs on the remote** (next to the workspace). The browser never spawns `copilot` locally (auth, files, and tools would be wrong machine).

Recommended: **daemon owns** the ACP subprocess and translates to ADE messages. Host UI remains a thin client (same as editor/PTY).

### 7.2 Transport

- Default: `copilot --acp --stdio` as a daemon-managed child (GitHub’s recommended IDE mode).
- Optional later: `--acp --port` on loopback for debugging.

### 7.3 ADE capability sketch

New capability: `agent` (name TBD). Illustrative messages (not normative until implementation):

| Message | Role |
|---------|------|
| `agent_start` / `agent_started` | Spawn ACP; return `agent_id`, advertised commands |
| `agent_prompt` | User text / slash command / attachments metadata |
| `agent_update` | Streamed chunks, tool calls, plans, mode |
| `agent_permission` / `agent_permission_reply` | Map ACP `requestPermission` |
| `agent_cancel` | Cancel turn |
| `agent_stop` | Tear down child |

Map ACP client filesystem/terminal callbacks to existing sandboxed FS and PTY APIs where possible, so the agent does not bypass `--root` without an explicit policy decision.

### 7.4 UI

- Open the reserved **right rail** for the agent panel (first real use of that region).
- Dense transcript: user / assistant / tool / plan; permission modal or inline approve.
- Keep terminal available: users can still fall back to interactive `copilot` TUI (Phase 0/1).

### 7.5 Auth and security

- Copilot auth stays with the CLI (`/login` or existing `gh` / Copilot credentials on the remote user). fresh-gui does not store GitHub tokens for Copilot.
- Autopilot / `--allow-all` must be opt-in and documented (same risk as shell YOLO).
- ACP file writes must honor FS sandbox or require an explicit “agent may write outside root” setting (default: sandbox only).
- See [SECURITY.md](./SECURITY.md) for token/SSH posture; agent subprocess is another local peer on the remote user account.

### 7.6 ACP preview risk

ACP in Copilot CLI is public preview and may change. Isolate the bridge behind the `agent` capability so protocol churn does not break core PTY/editor.

## 8. Autocomplete (explicit deferral)

For issue #49’s autocomplete ask:

1. **Do not** route typing through Copilot CLI or ACP prompts.  
2. **Do** track a separate design: expose Fresh’s existing LSP + completion services over ADE (tick loop, completion items, diagnostics) into CodeMirror. That reuses Fresh’s multi-server LSP and completion ranking — consistent with the Fresh-first rule.  
3. If GitHub later ships a headless completions API suitable for non-VS Code editors, evaluate it as an optional completion *provider* beside LSP — not as a substitute for Phase 1–2 agent work.

## 9. Architecture decision record

| ID | Choice | Why |
|----|--------|-----|
| **C1** | Agentic first; autocomplete separate | Matches what Copilot CLI actually is |
| **C2** | Phase 1 = PTY launcher, not chat rail | Terminal-first; minimal protocol change; Fresh Orchestrator pattern |
| **C3** | Phase 2 = ACP via daemon, not browser-spawned CLI | Correct machine, sandbox, lifecycle |
| **C4** | Do not load Fresh Orchestrator plugin into ADE yet | Terminal/window model mismatch; steal patterns instead |
| **C5** | No Copilot credentials in fresh-gui | CLI owns auth; smaller security surface |
| **C6** | Soften “no AI” non-goal to “no Terax-parity AI chrome” | Allows terminal/ACP agents without committing to Terax feature set |

## 10. Open questions

1. Should Phase 1 live entirely in the host (palette → `pty_open` with argv) or add a first-class backend agent registry?  
2. After agent file writes, is silent reload, prompt-to-reload, or Fresh buffer invalidate the right default?  
3. For Phase 2, should permission prompts be modal (blocking) or inline in the rail?  
4. Is Copilot the only Phase 1 preset, or should the registry ship with Claude/Codex stubs from day one?  
5. Licensing/product: document that Copilot requires a Copilot subscription and org CLI policy; fresh-gui remains GPL and does not redistribute Copilot.

## 11. Implementation sketch (when approved)

Not started. Likely touch points:

- Docs: this file + README tip (Phase 0).  
- Phase 1: `ui` palette + `pty_open` argv; optional `agents` in `config.rs` / `Hello.ui`.  
- Phase 2: `fresh-gui-protocol` agent messages; daemon ACP supervisor; right rail React panel; FS permission policy.  
- Tests: spawn failure; permission round-trip; sandbox write denial; no Copilot network calls in CI (mock ACP).

## 12. References

- Issue: https://github.com/amirhosseindavoody/fresh-gui/issues/49  
- Copilot CLI: https://github.com/features/copilot/cli/  
- ACP server: https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server  
- ACP overview: https://agentclientprotocol.com/protocol/overview  
- Fresh Orchestrator: `vendor/fresh/crates/fresh-editor/plugins/orchestrator.ts`  
- Fresh agent CLI plan: `vendor/fresh/docs/internal/agent-fresh-cli-exposure-plan.md`
