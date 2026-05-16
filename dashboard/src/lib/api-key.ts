// Browser-side persistence of the agent's X-ADOS-Key.
//
// Same-origin browser requests are trusted by the agent without a header
// (the agent's auth middleware accepts any request whose Origin matches
// its own listener). The stored key is only consulted when the dashboard
// is accessed cross-origin (Cloudflare Tunnel, reverse proxy, etc.) where
// the Origin doesn't match.
//
// The key is captured one of two ways:
//   1. The operator pastes it in Settings → Cloud and clicks Apply.
//   2. A one-shot URL parameter (?ados_key=…) is consumed on first load
//      and immediately removed from the address bar.

const STORAGE_KEY = "ados-api-key";
const URL_PARAM = "ados_key";

let cached: string | null | undefined;

function isBrowser(): boolean {
  return typeof window !== "undefined" && typeof localStorage !== "undefined";
}

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

export function setApiKey(value: string | null): void {
  cached = value && value.trim() ? value.trim() : null;
  if (!isBrowser()) return;
  try {
    if (cached) localStorage.setItem(STORAGE_KEY, cached);
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    // Storage may be disabled in private browsing; silently no-op.
  }
}

// Consume a one-shot ?ados_key=… URL parameter on first load. Called
// once from main.tsx before the React tree mounts so a tunnel link can
// transport the key without a manual paste.
export function consumeUrlKey(): void {
  if (!isBrowser()) return;
  try {
    const url = new URL(window.location.href);
    const incoming = url.searchParams.get(URL_PARAM);
    if (!incoming) return;
    setApiKey(incoming);
    url.searchParams.delete(URL_PARAM);
    window.history.replaceState({}, "", url.toString());
  } catch {
    // Malformed URLs are ignored.
  }
}
