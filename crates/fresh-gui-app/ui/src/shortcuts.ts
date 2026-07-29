/** Terax-aligned shortcut registry + dispatcher. */

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
  | "commandPalette.open";

export type KeyBinding = {
  key: string;
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
  meta?: boolean;
};

export type Shortcut = {
  id: ShortcutId;
  label: string;
  defaultBindings: KeyBinding[];
};

const isMac =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);

/** Mod key property used in bindings (Cmd on macOS, Ctrl elsewhere). */
export const MOD_PROP: "meta" | "ctrl" = isMac ? "meta" : "ctrl";

export const SHORTCUTS: Shortcut[] = [
  {
    id: "commandPalette.open",
    label: "Open command palette",
    defaultBindings: [{ [MOD_PROP]: true, key: "p" }],
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
];

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

export function matchShortcut(e: KeyboardEvent): ShortcutId | null {
  for (const shortcut of SHORTCUTS) {
    for (const binding of shortcut.defaultBindings) {
      if (matchBinding(e, binding)) return shortcut.id;
    }
  }
  return null;
}

export type ShortcutHandlers = Partial<Record<ShortcutId, (e: KeyboardEvent) => void>>;

export function installShortcuts(handlers: ShortcutHandlers): () => void {
  const onKeyDown = (e: KeyboardEvent) => {
    const target = e.target as HTMLElement | null;
    if (
      target &&
      (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
    ) {
      // Still allow save / palette / sidebar from inputs in the strip.
      const id = matchShortcut(e);
      if (id !== "editor.save" && id !== "commandPalette.open" && id !== "sidebar.toggle") {
        return;
      }
    }
    const id = matchShortcut(e);
    if (!id) return;
    const handler = handlers[id];
    if (!handler) return;
    e.preventDefault();
    handler(e);
  };
  window.addEventListener("keydown", onKeyDown);
  return () => window.removeEventListener("keydown", onKeyDown);
}
