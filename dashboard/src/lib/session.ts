// Browser-side persistence of the dashboard-access session token.
//
// A paired agent's own dashboard, reached from off-box, is unlocked by entering
// its PIN, which mints a short-lived session token (see the agent's
// `dashboard_session` module). The dashboard stores it here and sends it on
// every request as `X-ADOS-Dashboard-Session`; the native control front accepts
// it as an alternative data-plane credential to `X-ADOS-Key`.
//
// Unlike the API key, the session is scoped + expiring + revocable: a "reset
// PIN" from Mission Control rotates the salt the token is keyed with, so a stale
// token stops verifying and the dashboard drops back to the PIN splash.

const STORAGE_KEY = "ados-dashboard-session";

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
  // Drop a token past its expiry so a guaranteed-401 is not sent (the server
  // enforces expiry too; this just avoids the round-trip + the splash flash).
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
    // Storage may be disabled in private browsing; silently no-op.
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
