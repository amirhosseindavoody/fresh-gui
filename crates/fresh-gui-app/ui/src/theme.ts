/** Theme tokens + persistence (dark default, light optional). */

export type ThemeId = "dark" | "light";

const THEME_KEY = "fresh-gui.theme";

export function getTheme(): ThemeId {
  const v = localStorage.getItem(THEME_KEY);
  return v === "light" ? "light" : "dark";
}

export function applyTheme(theme: ThemeId): void {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem(THEME_KEY, theme);
}

export function toggleTheme(): ThemeId {
  const next: ThemeId = getTheme() === "dark" ? "light" : "dark";
  applyTheme(next);
  return next;
}

export function initTheme(): ThemeId {
  const theme = getTheme();
  applyTheme(theme);
  return theme;
}
