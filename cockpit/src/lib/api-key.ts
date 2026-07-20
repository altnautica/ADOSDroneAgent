// Optional API-key persistence. On-box (the normal case) the cockpit runs
// from localhost, a trusted origin, and needs no credential. When the panel
// is reached off-box (dev against a real agent, a tunnel link), a
// `?ados_key=…` URL parameter is captured once into localStorage and sent on
// requests + WS-ticket mints, mirroring the laptop dashboard.

const STORAGE_KEY = "ados-cockpit-api-key";
const URL_PARAM = "ados_key";

function isBrowser(): boolean {
  return typeof window !== "undefined" && typeof localStorage !== "undefined";
}

/** The stored API key, or null when none is set. */
export function getApiKey(): string | null {
  if (!isBrowser()) return null;
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

/** Capture a one-shot `?ados_key=…` URL parameter into storage, then strip it
 *  from the address bar so it is not left in history. No-op when absent. */
export function consumeUrlKey(): void {
  if (!isBrowser()) return;
  try {
    const url = new URL(window.location.href);
    const key = url.searchParams.get(URL_PARAM);
    if (!key) return;
    localStorage.setItem(STORAGE_KEY, key);
    url.searchParams.delete(URL_PARAM);
    window.history.replaceState(null, "", url.toString());
  } catch {
    // Storage disabled / malformed URL — ignore.
  }
}
