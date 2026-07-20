// Optional dashboard-session token persistence, mirroring the laptop
// dashboard. On-box the cockpit is trusted and never mints one; off-box a
// PIN-minted `dashboard_session` token (see the agent's `dashboard_session`
// module) is stored here and sent as `X-ADOS-Dashboard-Session`, the native
// control front's alternative data-plane credential to `X-ADOS-Key`.

const STORAGE_KEY = "ados-cockpit-session";

interface StoredSession {
  token: string;
  // Unix SECONDS (the agent's `expires_at`), 0 when unknown.
  expiresAt: number;
}

let cached: StoredSession | null | undefined;

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
    const parsed = JSON.parse(raw) as StoredSession;
    cached = parsed && typeof parsed.token === "string" ? parsed : null;
  } catch {
    cached = null;
  }
  return cached;
}

/** The current session token, or null when absent or client-side-expired. */
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
  if (!isBrowser()) return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(cached));
  } catch {
    // Storage may be disabled; silently no-op.
  }
}

export function clearSession(): void {
  cached = null;
  if (!isBrowser()) return;
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // no-op
  }
}
