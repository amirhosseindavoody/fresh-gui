/** Persist the bearer token across reloads after `?token=` is stripped from the URL. */

/** sessionStorage survives reload in this tab; cleared when the tab closes. */
export const AUTH_TOKEN_KEY = "fresh-gui.authToken";

export function loadCachedAuthToken(): string | null {
  try {
    const token = sessionStorage.getItem(AUTH_TOKEN_KEY);
    return token && token.trim() ? token : null;
  } catch {
    return null;
  }
}

export function cacheAuthToken(token: string): void {
  const trimmed = token.trim();
  if (!trimmed) return;
  try {
    sessionStorage.setItem(AUTH_TOKEN_KEY, trimmed);
  } catch {
    /* private mode / quota — reconnect-on-reload is best-effort */
  }
}

export function clearCachedAuthToken(): void {
  try {
    sessionStorage.removeItem(AUTH_TOKEN_KEY);
  } catch {
    /* ignore */
  }
}

/**
 * Read `?token=` from the startup banner URL, cache it for reloads, then strip
 * the query string from the address bar. Returns the token when present.
 */
export function consumeTokenQueryParam(): string | null {
  try {
    const params = new URLSearchParams(window.location.search);
    const token = params.get("token");
    if (!token) return null;
    cacheAuthToken(token);
    const next = `${window.location.pathname}${window.location.hash}`;
    window.history.replaceState({}, "", next);
    return token;
  } catch {
    return null;
  }
}
