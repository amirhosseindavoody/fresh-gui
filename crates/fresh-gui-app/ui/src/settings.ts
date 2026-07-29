/** Host UI settings parsed from backend `config.json` (`ui` section). */

import {
  applyTheme,
  getThemePreference,
  parseThemePreference,
  type ThemePreference,
} from "./theme";

const SETTINGS_KEY = "fresh-gui.settings";

export type UiSettings = {
  /** Stored preference; resolve with `resolveTheme` for chrome / xterm. */
  theme: ThemePreference;
  terminalFontSize: number;
  editorFontSize: number;
  webgl: boolean;
  /** Show names starting with `.` (except `.git`, controlled by `showGitDirs`). */
  showDotfiles: boolean;
  /** Show `.git` directories. Independent of `showDotfiles`; default off. */
  showGitDirs: boolean;
};

export type HelloUi = {
  theme?: string;
  terminalFontSize?: number;
  editorFontSize?: number;
  webgl?: boolean;
  showDotfiles?: boolean;
  showGitDirs?: boolean;
};

const DEFAULTS: UiSettings = {
  theme: "system",
  terminalFontSize: 14,
  editorFontSize: 14,
  webgl: true,
  showDotfiles: false,
  showGitDirs: false,
};

export function defaultUiSettings(): UiSettings {
  return { ...DEFAULTS };
}

function clamp(n: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, n));
}

function readBool(
  partial: Partial<UiSettings> | HelloUi | null | undefined,
  key: "webgl" | "showDotfiles" | "showGitDirs",
  fallback: boolean,
): boolean {
  if (!partial || !(key in partial)) return fallback;
  const value = (partial as Record<string, unknown>)[key];
  if (value == null) return fallback;
  return value !== false;
}

export function normalizeUiSettings(partial: Partial<UiSettings> | HelloUi | null | undefined): UiSettings {
  const themeRaw =
    partial && "theme" in partial && partial.theme != null
      ? partial.theme
      : DEFAULTS.theme;
  const term =
    partial && "terminalFontSize" in partial && partial.terminalFontSize != null
      ? Number(partial.terminalFontSize)
      : DEFAULTS.terminalFontSize;
  const ed =
    partial && "editorFontSize" in partial && partial.editorFontSize != null
      ? Number(partial.editorFontSize)
      : DEFAULTS.editorFontSize;
  return {
    theme: parseThemePreference(themeRaw),
    terminalFontSize: clamp(Number.isFinite(term) ? term : DEFAULTS.terminalFontSize, 10, 28),
    editorFontSize: clamp(Number.isFinite(ed) ? ed : DEFAULTS.editorFontSize, 10, 28),
    webgl: readBool(partial, "webgl", DEFAULTS.webgl),
    showDotfiles: readBool(partial, "showDotfiles", DEFAULTS.showDotfiles),
    showGitDirs: readBool(partial, "showGitDirs", DEFAULTS.showGitDirs),
  };
}

/** Offline / pre-connect cache (localStorage). Prefer backend `config.json` when connected. */
export function loadSettings(): UiSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return { ...DEFAULTS, theme: getThemePreference() };
    return normalizeUiSettings(JSON.parse(raw) as Partial<UiSettings>);
  } catch {
    return { ...DEFAULTS, theme: getThemePreference() };
  }
}

export function saveSettings(s: UiSettings): void {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(s));
  applyTheme(s.theme);
}

/** Strip JSONC comments (same rules as the backend). */
export function stripJsonc(input: string): string {
  let out = "";
  let i = 0;
  let inString = false;
  let escape = false;
  while (i < input.length) {
    const c = input[i]!;
    if (inString) {
      out += c;
      if (escape) escape = false;
      else if (c === "\\") escape = true;
      else if (c === '"') inString = false;
      i += 1;
      continue;
    }
    if (c === '"') {
      inString = true;
      out += c;
      i += 1;
      continue;
    }
    if (c === "/" && i + 1 < input.length) {
      const next = input[i + 1]!;
      if (next === "/") {
        i += 2;
        while (i < input.length && input[i] !== "\n") i += 1;
        continue;
      }
      if (next === "*") {
        i += 2;
        while (i + 1 < input.length && !(input[i] === "*" && input[i + 1] === "/")) i += 1;
        i = Math.min(i + 2, input.length);
        continue;
      }
    }
    out += c;
    i += 1;
  }
  return out;
}

/** Parse `ui` from a full `config.json` document (JSONC ok). */
export function uiSettingsFromConfigText(text: string): UiSettings {
  const trimmed = text.trim();
  if (!trimmed) return defaultUiSettings();
  const parsed = JSON.parse(stripJsonc(trimmed)) as { ui?: Partial<UiSettings> };
  return normalizeUiSettings(parsed.ui);
}
