// Browser-side persistence of the dashboard-access session token.
//
// A paired agent reached off-box is unlocked with its PIN, which mints a
// short-lived session token. It is stored here and sent as
// `X-ADOS-Dashboard-Session`, the alternative data-plane credential to
// `X-ADOS-Key`. Dashboard and cockpit share one storage key, so a session
// minted on either surface works for both.
//
// Storage is re-read when another tab changes it (`storage` event), so signing
// in or out in one tab is reflected everywhere without a reload.

const STORAGE_KEY = "ados-dashboard-session";

interface StoredSession {
  token: string;
  /** Unix SECONDS (the agent's `expires_at`), 0 when unknown. */
  expiresAt: number;
}

type Listener = () => void;

let cached: StoredSession | null | undefined;
const listeners = new Set<Listener>();

function isBrowser(): boolean {
  return typeof window !== "undefined" && typeof localStorage !== "undefined";
}

function read(): StoredSession | null {
  if (cached !== undefined) return cached;
  if (!isBrowser()) {
    cached = null;
    return null;
  }
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      cached = null;
      return null;
    }
    const parsed = JSON.parse(raw) as Partial<StoredSession> | null;
    cached =
      parsed && typeof parsed.token === "string"
        ? { token: parsed.token, expiresAt: Number(parsed.expiresAt) || 0 }
        : null;
  } catch {
    cached = null;
  }
  return cached;
}

function notify(): void {
  for (const l of listeners) l();
}

/** The current session token, or null when absent or past its expiry. */
export function getSession(): string | null {
  const s = read();
  if (!s) return null;
  if (s.expiresAt && s.expiresAt * 1000 <= Date.now()) {
    clearSession();
    return null;
  }
  return s.token;
}

export function setSession(token: string, expiresAt: number): void {
  cached = { token, expiresAt };
  if (isBrowser()) {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(cached));
    } catch {
      // Storage may be disabled; the in-memory copy stands.
    }
  }
  notify();
}

export function clearSession(): void {
  const had = read() !== null;
  cached = null;
  if (isBrowser()) {
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      // no-op
    }
  }
  if (had) notify();
}

/** Subscribe to session changes (this tab or another). Returns an unsubscribe. */
export function onSessionChange(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

if (isBrowser()) {
  window.addEventListener("storage", (ev) => {
    if (ev.key !== null && ev.key !== STORAGE_KEY) return;
    cached = undefined;
    notify();
  });
}
