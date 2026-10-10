// Browser-side persistence of the agent's `X-ADOS-Key`.
//
// The dashboard and the cockpit are the same origin on the same agent, so one
// stored key serves both. A paired agent requires a credential on its data
// routes when reached off-box; on-box (localhost) none is needed and none is
// sent while nothing is stored.
//
// The key arrives on a one-shot URL parameter (a Mission Control deep link or
// the agent's own reach link), is captured once, and is removed from the
// address bar so it does not linger in history.

const STORAGE_KEY = "ados-api-key";
// Both spellings are in circulation: the agent's own redirect preserves
// `?key=`, while deep links use `?ados_key=`.
const URL_PARAMS = ["ados_key", "key"] as const;

let cached: string | null | undefined;

function isBrowser(): boolean {
  return typeof window !== "undefined" && typeof localStorage !== "undefined";
}

/** The stored API key, or null when none is set. */
export function getApiKey(): string | null {
  if (cached !== undefined) return cached;
  if (!isBrowser()) {
    cached = null;
    return null;
  }
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    cached = raw && raw.trim() ? raw.trim() : null;
  } catch {
    cached = null;
  }
  return cached;
}

/** Store (or, with null, remove) the API key. */
export function setApiKey(value: string | null): void {
  cached = value && value.trim() ? value.trim() : null;
  if (!isBrowser()) return;
  try {
    if (cached) localStorage.setItem(STORAGE_KEY, cached);
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    // Storage may be disabled (private browsing); the in-memory copy stands.
  }
}

/** Drop the in-memory copy so the next read goes back to storage. Used when
 *  another tab changed the stored value. */
export function invalidateApiKeyCache(): void {
  cached = undefined;
}

/** Capture a one-shot key from the URL into storage, then strip every accepted
 *  spelling from the address bar. No-op when neither is present. */
export function consumeUrlKey(): void {
  if (!isBrowser()) return;
  try {
    const url = new URL(window.location.href);
    const param = URL_PARAMS.find((p) => url.searchParams.get(p));
    if (!param) return;
    setApiKey(url.searchParams.get(param));
    for (const p of URL_PARAMS) url.searchParams.delete(p);
    window.history.replaceState(window.history.state, "", url.toString());
  } catch {
    // Malformed URL or storage disabled: ignore.
  }
}

if (isBrowser()) {
  window.addEventListener("storage", (ev) => {
    if (ev.key === null || ev.key === STORAGE_KEY) invalidateApiKeyCache();
  });
}
