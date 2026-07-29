/** Theme preference + resolved dark/light (system default). */

export type ThemePreference = "system" | "dark" | "light";
export type ResolvedTheme = "dark" | "light";

/** @deprecated Prefer {@link ThemePreference}; kept as alias for call sites. */
export type ThemeId = ThemePreference;

const THEME_KEY = "fresh-gui.theme";

type ThemeListener = (resolved: ResolvedTheme) => void;

const listeners = new Set<ThemeListener>();
let mediaQuery: MediaQueryList | null = null;
let mediaHandler: (() => void) | null = null;
let currentPreference: ThemePreference = "system";

export function parseThemePreference(value: unknown): ThemePreference {
  if (value === "light" || value === "dark" || value === "system") return value;
  return "system";
}

export function systemPrefersDark(): boolean {
  return (
    typeof matchMedia === "function" &&
    matchMedia("(prefers-color-scheme: dark)").matches
  );
}

export function resolveTheme(preference: ThemePreference = getThemePreference()): ResolvedTheme {
  if (preference === "light") return "light";
  if (preference === "dark") return "dark";
  return systemPrefersDark() ? "dark" : "light";
}

export function getThemePreference(): ThemePreference {
  try {
    return parseThemePreference(localStorage.getItem(THEME_KEY));
  } catch {
    return "system";
  }
}

/** Alias used by older call sites — returns the stored preference. */
export function getTheme(): ThemePreference {
  return getThemePreference();
}

export function getResolvedTheme(): ResolvedTheme {
  return resolveTheme(currentPreference);
}

export function onResolvedThemeChange(listener: ThemeListener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function notify(resolved: ResolvedTheme): void {
  for (const listener of listeners) {
    try {
      listener(resolved);
    } catch {
      /* ignore listener errors */
    }
  }
}

function syncMediaListener(preference: ThemePreference): void {
  if (typeof matchMedia !== "function") return;

  if (mediaQuery && mediaHandler) {
    mediaQuery.removeEventListener("change", mediaHandler);
    mediaQuery = null;
    mediaHandler = null;
  }

  if (preference !== "system") return;

  mediaQuery = matchMedia("(prefers-color-scheme: dark)");
  mediaHandler = () => {
    const resolved = resolveTheme("system");
    document.documentElement.dataset.theme = resolved;
    notify(resolved);
  };
  mediaQuery.addEventListener("change", mediaHandler);
}

/** Persist preference and apply the resolved theme to `documentElement`. */
export function applyTheme(preference: ThemePreference): ResolvedTheme {
  currentPreference = preference;
  try {
    localStorage.setItem(THEME_KEY, preference);
  } catch {
    /* private mode */
  }
  const resolved = resolveTheme(preference);
  document.documentElement.dataset.theme = resolved;
  syncMediaListener(preference);
  notify(resolved);
  return resolved;
}

export function toggleTheme(): ThemePreference {
  const order: ThemePreference[] = ["system", "light", "dark"];
  const i = order.indexOf(getThemePreference());
  const next = order[(i + 1) % order.length]!;
  applyTheme(next);
  return next;
}

export function initTheme(): ResolvedTheme {
  return applyTheme(getThemePreference());
}

/** xterm palette derived from CSS tokens on `:root` (follows `data-theme`). */
export function xtermThemeFromCss(): {
  background: string;
  foreground: string;
  cursor: string;
  selectionBackground: string;
  selectionInactiveBackground: string;
  selectionForeground: string;
} {
  const s = getComputedStyle(document.documentElement);
  const read = (name: string, fallback: string) => {
    const v = s.getPropertyValue(name).trim();
    return v || fallback;
  };
  const selection = read("--term-selection", "#2da44ea6");
  return {
    background: read("--term-bg", read("--bg", "#010409")),
    foreground: read("--term-fg", read("--text", "#e6edf3")),
    cursor: read("--term-cursor", read("--accent", "#3fb950")),
    selectionBackground: selection,
    selectionInactiveBackground: selection,
    selectionForeground: read("--term-selection-fg", read("--term-fg", "#1f2328")),
  };
}

/** Prefer the loaded `--font-mono` token (IBM Plex Mono via @fontsource). */
export function monoFontFromCss(): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim();
  return v || '"IBM Plex Mono", ui-monospace, monospace';
}

/** Regular mono weight from `--font-mono-weight` (default 400). */
export function monoFontWeightFromCss(): number {
  const v = getComputedStyle(document.documentElement).getPropertyValue("--font-mono-weight").trim();
  const n = Number(v);
  return Number.isFinite(n) ? n : 400;
}

/** Bold mono weight from `--font-mono-weight-bold` (default 600). */
export function monoFontWeightBoldFromCss(): number {
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue("--font-mono-weight-bold")
    .trim();
  const n = Number(v);
  return Number.isFinite(n) ? n : 600;
}
