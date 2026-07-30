/** Host chrome shortcut registry + dispatcher (bindings from config / Hello). */

export type ShortcutId =
  | "tab.new"
  | "tab.close"
  | "tab.next"
  | "tab.prev"
  | "pane.splitRight"
  | "pane.splitDown"
  | "pane.focusNext"
  | "pane.focusPrev"
  | "sidebar.toggle"
  | "editor.save"
  | "editor.markdownPreview"
  | "editor.toggleLineWrap"
  | "commandPalette.open"
  | "gotoFile.open"
  | "search.focus"
  | "settings.open"
  | "settings.openDefaults";

export type KeyBinding = {
  key: string;
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
  meta?: boolean;
};

/** Fresh-style when context for host chrome. */
export type ShortcutWhen = "global" | "terminal" | "editor" | "fileExplorer" | string;

export type ShortkeyEntry = {
  action: string;
  shortkey: string;
  when?: string;
};

export type Shortcut = {
  id: ShortcutId;
  label: string;
  /** Built-in bindings used before Hello / when config has no shortkeys. */
  defaultBindings: KeyBinding[];
};

const isMac =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);

/** Mod key property used in bindings (Cmd on macOS, Ctrl elsewhere). */
export const MOD_PROP: "meta" | "ctrl" = isMac ? "meta" : "ctrl";

/** Sentinel path the backend materializes as a temp defaults catalog. */
export const DEFAULT_SETTINGS_OPEN_PATH = "fresh-gui://defaults/config.json";

export const SHORTCUTS: Shortcut[] = [
  {
    id: "gotoFile.open",
    label: "Go to File",
    defaultBindings: [{ [MOD_PROP]: true, key: "p" }],
  },
  {
    id: "commandPalette.open",
    label: "Open command palette",
    defaultBindings: [{ [MOD_PROP]: true, shift: true, key: "p" }],
  },
  {
    id: "tab.new",
    label: "New terminal tab",
    defaultBindings: [{ [MOD_PROP]: true, key: "t" }],
  },
  {
    id: "tab.close",
    label: "Close tab or pane",
    defaultBindings: [{ [MOD_PROP]: true, key: "w" }],
  },
  {
    id: "tab.next",
    label: "Next tab",
    defaultBindings: [{ ctrl: true, key: "Tab" }],
  },
  {
    id: "tab.prev",
    label: "Previous tab",
    defaultBindings: [{ ctrl: true, shift: true, key: "Tab" }],
  },
  {
    id: "pane.splitRight",
    label: "Split pane right",
    defaultBindings: [{ [MOD_PROP]: true, key: "d" }],
  },
  {
    id: "pane.splitDown",
    label: "Split pane down",
    defaultBindings: [{ [MOD_PROP]: true, shift: true, key: "d" }],
  },
  {
    id: "pane.focusNext",
    label: "Focus next pane",
    defaultBindings: [{ [MOD_PROP]: true, key: "]" }],
  },
  {
    id: "pane.focusPrev",
    label: "Focus previous pane",
    defaultBindings: [{ [MOD_PROP]: true, key: "[" }],
  },
  {
    id: "sidebar.toggle",
    label: "Toggle sidebar",
    defaultBindings: [
      { [MOD_PROP]: true, key: "b" },
      { [MOD_PROP]: true, shift: true, key: "b" },
    ],
  },
  {
    id: "editor.save",
    label: "Save buffer",
    defaultBindings: [{ [MOD_PROP]: true, key: "s" }],
  },
  {
    id: "editor.markdownPreview",
    label: "Toggle markdown WYSIWYG",
    defaultBindings: [{ [MOD_PROP]: true, shift: true, key: "v" }],
  },
  {
    id: "editor.toggleLineWrap",
    label: "Toggle editor line wrap",
    // VS Code–style Alt+Z; aligns with Fresh ToggleLineWrap semantics.
    defaultBindings: [{ alt: true, key: "z" }],
  },
  {
    id: "search.focus",
    label: "Find in active pane",
    defaultBindings: [{ [MOD_PROP]: true, key: "f" }],
  },
  {
    id: "settings.open",
    label: "Open settings file",
    defaultBindings: [{ [MOD_PROP]: true, key: "," }],
  },
  {
    id: "settings.openDefaults",
    label: "Open default settings",
    defaultBindings: [],
  },
];

const SHORTCUT_IDS = new Set<string>(SHORTCUTS.map((s) => s.id));

export function isShortcutId(id: string): id is ShortcutId {
  return SHORTCUT_IDS.has(id);
}

/** Parse a chord like `Mod+Shift+P` / `Ctrl+Tab` / `Alt+Z` into a KeyBinding. */
export function parseShortkeyChord(chord: string): KeyBinding | null {
  const parts = chord
    .split("+")
    .map((p) => p.trim())
    .filter(Boolean);
  if (parts.length === 0) return null;

  const keyRaw = parts[parts.length - 1]!;
  const mods = parts.slice(0, -1).map((m) => m.toLowerCase());
  const binding: KeyBinding = {
    key: keyRaw.length === 1 ? keyRaw.toLowerCase() : keyRaw,
  };

  for (const mod of mods) {
    switch (mod) {
      case "mod":
        binding[MOD_PROP] = true;
        break;
      case "ctrl":
      case "control":
        binding.ctrl = true;
        break;
      case "shift":
        binding.shift = true;
        break;
      case "alt":
      case "option":
        binding.alt = true;
        break;
      case "meta":
      case "cmd":
      case "command":
      case "super":
        binding.meta = true;
        break;
      default:
        return null;
    }
  }
  return binding;
}

export type ActiveBinding = {
  id: ShortcutId;
  binding: KeyBinding;
  when: ShortcutWhen;
};

function defaultActiveBindings(): ActiveBinding[] {
  const out: ActiveBinding[] = [];
  for (const shortcut of SHORTCUTS) {
    for (const binding of shortcut.defaultBindings) {
      out.push({ id: shortcut.id, binding, when: "global" });
    }
  }
  return out;
}

/** Build the active binding table from config / Hello shortkeys (falls back to defaults). */
export function activeBindingsFromShortkeys(entries: ShortkeyEntry[] | null | undefined): ActiveBinding[] {
  if (!entries || entries.length === 0) return defaultActiveBindings();
  const out: ActiveBinding[] = [];
  for (const entry of entries) {
    if (!isShortcutId(entry.action)) continue;
    const binding = parseShortkeyChord(entry.shortkey);
    if (!binding) continue;
    const when = (entry.when?.trim() || "global") as ShortcutWhen;
    out.push({ id: entry.action, binding, when });
  }
  return out.length > 0 ? out : defaultActiveBindings();
}

let activeBindings: ActiveBinding[] = defaultActiveBindings();

/** Replace the live binding table (from Hello or saved config.json). */
export function setActiveShortkeys(entries: ShortkeyEntry[] | null | undefined): void {
  activeBindings = activeBindingsFromShortkeys(entries);
}

export function getActiveBindings(): readonly ActiveBinding[] {
  return activeBindings;
}

export function matchBinding(e: KeyboardEvent, binding: KeyBinding): boolean {
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  const want = binding.key.length === 1 ? binding.key.toLowerCase() : binding.key;
  if (key !== want) return false;
  return (
    !!e.ctrlKey === !!binding.ctrl &&
    !!e.shiftKey === !!binding.shift &&
    !!e.altKey === !!binding.alt &&
    !!e.metaKey === !!binding.meta
  );
}

export type ShortcutContext = {
  /** Active workspace surface. */
  surface: "terminal" | "editor" | "none";
  /** File explorer / tree has DOM focus. */
  fileExplorerFocused?: boolean;
};

function whenMatches(when: ShortcutWhen, ctx: ShortcutContext): boolean {
  const w = (when || "global").trim().toLowerCase();
  if (!w || w === "global") return true;
  if (w === "terminal") return ctx.surface === "terminal";
  if (w === "editor" || w === "normal") return ctx.surface === "editor";
  if (w === "fileexplorer" || w === "file_explorer") return !!ctx.fileExplorerFocused;
  return false;
}

export function matchShortcut(e: KeyboardEvent, ctx: ShortcutContext = { surface: "none" }): ShortcutId | null {
  for (const entry of activeBindings) {
    if (!whenMatches(entry.when, ctx)) continue;
    if (matchBinding(e, entry.binding)) return entry.id;
  }
  return null;
}

export type ShortcutHandlers = Partial<Record<ShortcutId, (e: KeyboardEvent) => void>>;

const INPUT_ALLOWLIST: ReadonlySet<ShortcutId> = new Set([
  "editor.save",
  "editor.markdownPreview",
  "editor.toggleLineWrap",
  "commandPalette.open",
  "gotoFile.open",
  "sidebar.toggle",
  "search.focus",
  "settings.open",
  "settings.openDefaults",
]);

export type InstallShortcutsOptions = {
  /** Resolve current focus context for `when` clauses. */
  getContext?: () => ShortcutContext;
};

export function installShortcuts(
  handlers: ShortcutHandlers,
  opts: InstallShortcutsOptions = {},
): () => void {
  const getContext = opts.getContext ?? (() => ({ surface: "none" as const }));
  const onKeyDown = (e: KeyboardEvent) => {
    const target = e.target as HTMLElement | null;
    const ctx = getContext();
    if (
      target &&
      (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
    ) {
      // Still allow save / palette / goto / sidebar from inputs in the strip.
      const id = matchShortcut(e, ctx);
      if (!id || !INPUT_ALLOWLIST.has(id)) {
        return;
      }
    }
    const id = matchShortcut(e, ctx);
    if (!id) return;
    const handler = handlers[id];
    if (!handler) return;
    e.preventDefault();
    handler(e);
  };
  window.addEventListener("keydown", onKeyDown);
  return () => window.removeEventListener("keydown", onKeyDown);
}
