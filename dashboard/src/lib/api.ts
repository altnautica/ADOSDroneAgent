// Thin wrapper around fetch for the agent's REST surface. All paths are
// relative ("/api/...") so the same code works behind Vite's dev proxy
// and against the real agent at runtime. JSON in, JSON out, throws on
// non-2xx with a useful message.

import { getApiKey } from "./api-key";
import { clearSession, getSession } from "./session";

export class ApiError extends Error {
  status: number;
  body: unknown;

  constructor(message: string, status: number, body: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

/**
 * Whether a failed response means "you are not authorized yet" rather than
 * "something is wrong".
 *
 * BOTH codes, deliberately. The agent uses them for two different situations
 * and the UI has to handle both the same way:
 *
 * - **401** — the node is PAIRED and we reached it off-box without a
 *   credential, or with an expired or revoked one.
 * - **403** — the node is UNPAIRED and we are on its LAN, so it is asking for
 *   the dashboard PIN before it hands over any data.
 *
 * Only 401 used to count. A fresh node is unpaired with no PIN set, which is
 * the 403 case, so every newly installed agent fell through to the generic
 * error path and the dashboard told the operator the board was unreachable —
 * about a board that had just answered, promptly, saying exactly what it
 * needed. The enrolment screen it should have shown already existed and was
 * simply never reached.
 */
export function isAuthChallenge(status: number): boolean {
  return status === 401 || status === 403;
}

interface FetchOptions {
  method?: "GET" | "POST" | "PUT" | "DELETE";
  body?: unknown;
  signal?: AbortSignal;
  // Set by the access gate's own probe so a 401 there does NOT re-notify the
  // gate (which would recurse). Panel fetches leave this unset so a mid-session
  // 401 hands the UI back to the gate.
  skipAuthSignal?: boolean;
}

// The access gate registers a handler here. On a data-plane 401 (a paired agent
// reached off-box with no/expired/revoked credential) `apiFetch` drops the stale
// session and notifies the gate, which shows the branded PIN splash instead of a
// blank dashboard. Replaces the old raw-key `window.prompt`.
type AuthRequiredHandler = () => void;
let authRequiredHandler: AuthRequiredHandler | null = null;

export function setAuthRequiredHandler(fn: AuthRequiredHandler | null): void {
  authRequiredHandler = fn;
}

export async function apiFetch<T = unknown>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };

  // A paired agent requires a data-plane credential off-box. Prefer the
  // dashboard session (minted by the PIN gate); also send the API key when one
  // is stored (the Mission Control `?ados_key=` deep-link + Settings → Cloud
  // path still works untouched).
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const storedKey = getApiKey();
  if (storedKey) headers["X-ADOS-Key"] = storedKey;

  const init: RequestInit = {
    method: opts.method ?? "GET",
    headers,
    signal: opts.signal,
  };

  if (opts.body !== undefined) {
    (init.headers as Record<string, string>)["Content-Type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }

  const res = await fetch(path, init);

  // Direct-visit auth: the agent is telling us we are not authorized yet —
  // either a paired node missing a credential (401) or an unpaired node asking
  // for its PIN (403). Drop any stale session and hand the UI to the access
  // gate (which shows the PIN splash). The gate's own probe passes
  // `skipAuthSignal` so it resolves the challenge itself without recursing.
  if (isAuthChallenge(res.status) && !opts.signal?.aborted) {
    clearSession();
    if (!opts.skipAuthSignal) authRequiredHandler?.();
  }

  let body: unknown = null;
  const text = await res.text();
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = text;
    }
  }

  if (!res.ok) {
    const detail =
      body && typeof body === "object" && "detail" in body
        ? String((body as { detail: unknown }).detail)
        : res.statusText;
    throw new ApiError(`${res.status} ${detail}`, res.status, body);
  }

  return body as T;
}
