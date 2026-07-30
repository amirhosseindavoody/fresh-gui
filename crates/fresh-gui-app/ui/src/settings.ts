/** Host UI settings parsed from backend `config.json` (`ui` section). */

import { applyPalette, parsePaletteId, type PaletteId } from "./palettes";
import {
  applyTheme,
  getThemePreference,
  parseThemePreference,
  type ThemePreference,
} from "./theme";

const SETTINGS_KEY = "fresh-gui.settings";

const DEFAULT_UI_FONT = '"IBM Plex Sans", "Segoe UI", system-ui, sans-serif';
const DEFAULT_MONO_FONT = '"IBM Plex Mono", ui-monospace, monospace';

export type UiSettings = {
  /** Stored preference; resolve with `resolveTheme` for chrome / xterm when palette is primer. */
  theme: ThemePreference;
  /**
   * Color pack. `primer` follows `theme` (system/light/dark).
   * Other ids match Fresh editor theme names (mapped onto host CSS tokens).
   */
  palette: PaletteId;
  terminalFontSize: number;
  editorFontSize: number;
  /** UI chrome font weight (100–900). */
  fontWeight: number;
  /** Terminal + editor monospace weight (100–900). */
  monoFontWeight: number;
  /** Optional UI font family CSS value; empty → IBM Plex Sans. */
  fontFamily: string;
  /** Optional mono font family CSS value; empty → IBM Plex Mono. */
  monoFontFamily: string;
  webgl: boolean;
  /** Show names starting with `.` (except `.git`, controlled by `showGitDirs`). */
  showDotfiles: boolean;
  /** Show `.git` directories. Independent of `showDotfiles`; default off. */
  showGitDirs: boolean;
  /**
   * VS Code–style editor document map (minimap). Off by default; when false the
   * minimap module is never loaded.
   */
  editorMinimap: boolean;
  /**
   * Soft-wrap long lines in the editor (Fresh `editor.line_wrap`). Default on.
   */
  editorLineWrap: boolean;
};

export type HelloUi = {
  theme?: string;
  palette?: string;
  terminalFontSize?: number;
  editorFontSize?: number;
  fontWeight?: number;
  monoFontWeight?: number;
  fontFamily?: string;
  monoFontFamily?: string;
  webgl?: boolean;
  showDotfiles?: boolean;
  showGitDirs?: boolean;
  editorMinimap?: boolean;
  editorLineWrap?: boolean;
};

const DEFAULTS: UiSettings = {
  theme: "system",
  palette: "primer",
  terminalFontSize: 14,
  editorFontSize: 14,
  fontWeight: 400,
  monoFontWeight: 400,
  fontFamily: "",
  monoFontFamily: "",
  webgl: true,
  showDotfiles: false,
  showGitDirs: false,
  editorMinimap: false,
  editorLineWrap: true,
};

export function defaultUiSettings(): UiSettings {
  return { ...DEFAULTS };
}

function clamp(n: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, n));
}

function readBool(
  partial: Partial<UiSettings> | HelloUi | null | undefined,
  key: "webgl" | "showDotfiles" | "showGitDirs" | "editorMinimap" | "editorLineWrap",
  fallback: boolean,
): boolean {
  if (!partial || !(key in partial)) return fallback;
  const value = (partial as Record<string, unknown>)[key];
  if (value == null) return fallback;
  return value !== false;
}

function readNumber(
  partial: Partial<UiSettings> | HelloUi | null | undefined,
  key: keyof UiSettings,
  fallback: number,
): number {
  if (!partial || !(key in partial)) return fallback;
  const value = (partial as Record<string, unknown>)[key];
  if (value == null) return fallback;
  const n = Number(value);
  return Number.isFinite(n) ? n : fallback;
}

function readString(
  partial: Partial<UiSettings> | HelloUi | null | undefined,
  key: "fontFamily" | "monoFontFamily",
  fallback: string,
): string {
  if (!partial || !(key in partial)) return fallback;
  const value = (partial as Record<string, unknown>)[key];
  if (typeof value !== "string") return fallback;
  return value.trim();
}

/** Snap font weight to a CSS-friendly 100-step in 100–900. */
export function normalizeFontWeight(value: unknown, fallback = 400): number {
  const n = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(n)) return fallback;
  const clamped = clamp(Math.round(n), 100, 900);
  return Math.round(clamped / 100) * 100;
}

export function normalizeUiSettings(partial: Partial<UiSettings> | HelloUi | null | undefined): UiSettings {
  const themeRaw =
    partial && "theme" in partial && partial.theme != null
      ? partial.theme
      : DEFAULTS.theme;
  const paletteRaw =
    partial && "palette" in partial && partial.palette != null
      ? partial.palette
      : DEFAULTS.palette;
  return {
    theme: parseThemePreference(themeRaw),
    palette: parsePaletteId(paletteRaw),
    terminalFontSize: clamp(
      readNumber(partial, "terminalFontSize", DEFAULTS.terminalFontSize),
      10,
      28,
    ),
    editorFontSize: clamp(
      readNumber(partial, "editorFontSize", DEFAULTS.editorFontSize),
      10,
      28,
    ),
    fontWeight: normalizeFontWeight(
      partial && "fontWeight" in partial ? partial.fontWeight : DEFAULTS.fontWeight,
      DEFAULTS.fontWeight,
    ),
    monoFontWeight: normalizeFontWeight(
      partial && "monoFontWeight" in partial ? partial.monoFontWeight : DEFAULTS.monoFontWeight,
      DEFAULTS.monoFontWeight,
    ),
    fontFamily: readString(partial, "fontFamily", DEFAULTS.fontFamily),
    monoFontFamily: readString(partial, "monoFontFamily", DEFAULTS.monoFontFamily),
    webgl: readBool(partial, "webgl", DEFAULTS.webgl),
    showDotfiles: readBool(partial, "showDotfiles", DEFAULTS.showDotfiles),
    showGitDirs: readBool(partial, "showGitDirs", DEFAULTS.showGitDirs),
    editorMinimap: readBool(partial, "editorMinimap", DEFAULTS.editorMinimap),
    editorLineWrap: readBool(partial, "editorLineWrap", DEFAULTS.editorLineWrap),
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

/** Apply palette + typography CSS vars from settings (does not persist). */
export function applyUiChrome(settings: UiSettings): void {
  const root = document.documentElement;
  applyTheme(settings.theme);
  const forced = applyPalette(settings.palette, root);
  // Named Fresh packs pin appearance; primer keeps the resolved theme mode.
  if (forced) {
    root.dataset.theme = forced;
  }

  const uiFont = settings.fontFamily || DEFAULT_UI_FONT;
  const monoFont = settings.monoFontFamily || DEFAULT_MONO_FONT;
  root.style.setProperty("--font-ui", uiFont);
  root.style.setProperty("--font-mono", monoFont);
  root.style.setProperty("--font-ui-weight", String(settings.fontWeight));
  root.style.setProperty("--font-mono-weight", String(settings.monoFontWeight));
  // Bold faces sit two steps above the regular mono weight (capped).
  const bold = Math.min(900, settings.monoFontWeight + 200);
  root.style.setProperty("--font-mono-weight-bold", String(bold));
}

export function saveSettings(s: UiSettings): void {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(s));
  applyUiChrome(s);
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

export type ConfigShortkey = {
  action: string;
  shortkey: string;
  when?: string;
};

/** Parse `shortkeys` from a full `config.json` document (JSONC ok). */
export function shortkeysFromConfigText(text: string): ConfigShortkey[] {
  const trimmed = text.trim();
  if (!trimmed) return [];
  const parsed = JSON.parse(stripJsonc(trimmed)) as { shortkeys?: unknown };
  if (!Array.isArray(parsed.shortkeys)) return [];
  const out: ConfigShortkey[] = [];
  for (const item of parsed.shortkeys) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    const action = typeof row.action === "string" ? row.action.trim() : "";
    const shortkey = typeof row.shortkey === "string" ? row.shortkey.trim() : "";
    if (!action || !shortkey) continue;
    const when =
      typeof row.when === "string" && row.when.trim() ? row.when.trim() : undefined;
    out.push({ action, shortkey, when });
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

export { DEFAULT_UI_FONT, DEFAULT_MONO_FONT };
