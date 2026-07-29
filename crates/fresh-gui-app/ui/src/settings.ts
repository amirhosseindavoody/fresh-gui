/** Host UI settings (fonts / renderer / theme) — web modal for browser + Tauri. */

import { applyTheme, getTheme, type ThemeId } from "./theme";

const SETTINGS_KEY = "fresh-gui.settings";

export type UiSettings = {
  theme: ThemeId;
  terminalFontSize: number;
  editorFontSize: number;
  webgl: boolean;
};

const DEFAULTS: UiSettings = {
  theme: "dark",
  terminalFontSize: 14,
  editorFontSize: 14,
  webgl: true,
};

let root: HTMLElement | null = null;
let onChange: ((s: UiSettings) => void) | null = null;

export function loadSettings(): UiSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return { ...DEFAULTS, theme: getTheme() };
    const parsed = JSON.parse(raw) as Partial<UiSettings>;
    return {
      theme: parsed.theme === "light" ? "light" : "dark",
      terminalFontSize: clamp(parsed.terminalFontSize ?? DEFAULTS.terminalFontSize, 10, 28),
      editorFontSize: clamp(parsed.editorFontSize ?? DEFAULTS.editorFontSize, 10, 28),
      webgl: parsed.webgl !== false,
    };
  } catch {
    return { ...DEFAULTS, theme: getTheme() };
  }
}

export function saveSettings(s: UiSettings): void {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(s));
  applyTheme(s.theme);
}

function clamp(n: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, n));
}

export function setSettingsChangeHandler(handler: (s: UiSettings) => void): void {
  onChange = handler;
}

function ensureDom(): void {
  if (root) return;
  root = document.createElement("div");
  root.className = "settings-modal";
  root.hidden = true;
  root.innerHTML = `
    <div class="settings-backdrop" data-close="1"></div>
    <div class="settings-panel" role="dialog" aria-label="Settings">
      <header class="settings-head">
        <h2>Settings</h2>
        <button type="button" class="settings-close" data-close="1" title="Close">×</button>
      </header>
      <div class="settings-body">
        <label>Theme
          <select id="settings-theme">
            <option value="dark">Dark</option>
            <option value="light">Light</option>
          </select>
        </label>
        <label>Terminal font size
          <input id="settings-term-font" type="number" min="10" max="28" step="1" />
        </label>
        <label>Editor font size
          <input id="settings-editor-font" type="number" min="10" max="28" step="1" />
        </label>
        <label class="settings-check">
          <input id="settings-webgl" type="checkbox" />
          Prefer xterm WebGL renderer
        </label>
      </div>
      <footer class="settings-foot">
        <button type="button" class="primary" id="settings-save">Save</button>
      </footer>
    </div>
  `;
  document.body.appendChild(root);
  root.addEventListener("click", (ev) => {
    const t = ev.target as HTMLElement;
    if (t.dataset.close === "1") closeSettings();
  });
  root.querySelector("#settings-save")?.addEventListener("click", () => {
    const next = readForm();
    saveSettings(next);
    onChange?.(next);
    closeSettings();
  });
}

function readForm(): UiSettings {
  const themeEl = document.getElementById("settings-theme") as HTMLSelectElement;
  const termEl = document.getElementById("settings-term-font") as HTMLInputElement;
  const edEl = document.getElementById("settings-editor-font") as HTMLInputElement;
  const webglEl = document.getElementById("settings-webgl") as HTMLInputElement;
  return {
    theme: themeEl.value === "light" ? "light" : "dark",
    terminalFontSize: clamp(Number(termEl.value) || 14, 10, 28),
    editorFontSize: clamp(Number(edEl.value) || 14, 10, 28),
    webgl: webglEl.checked,
  };
}

function fillForm(s: UiSettings): void {
  (document.getElementById("settings-theme") as HTMLSelectElement).value = s.theme;
  (document.getElementById("settings-term-font") as HTMLInputElement).value = String(s.terminalFontSize);
  (document.getElementById("settings-editor-font") as HTMLInputElement).value = String(s.editorFontSize);
  (document.getElementById("settings-webgl") as HTMLInputElement).checked = s.webgl;
}

export function openSettings(): void {
  ensureDom();
  if (!root) return;
  fillForm(loadSettings());
  root.hidden = false;
}

export function closeSettings(): void {
  if (root) root.hidden = true;
}
