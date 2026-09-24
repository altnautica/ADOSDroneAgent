// Thin wrapper around fetch for the agent's REST surface. All paths are
// relative ("/api/...") so the same code works behind Vite's dev proxy
// and against the real agent at runtime. JSON in, JSON out, throws on
// non-2xx with a useful message.

import { getApiKey } from "./api-key";
import { getSession } from "./session";

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
  /** JSON-encoded, except a FormData body, which is sent as multipart. */
  body?: unknown;
  signal?: AbortSignal;
  // Set by the access gate's own probe so a 401 there does NOT re-notify the
  // gate (which would recurse). Panel fetches leave this unset so a mid-session
  // 401 hands the UI back to the gate.
  skipAuthSignal?: boolean;
}

// The access gate registers a handler here. On a 401/403 `apiFetch` asks the
// gate to re-verify access; the gate's own probe decides whether the stored
// session is dead (and only then drops it and shows the PIN splash). A 403 is
// not always about the session — a capability-denied plugin call, a
// relay-forbidden path or an MCP scope refusal are all 403s from an agent that
// accepted the credential — so a refusal must never log the operator out by
// itself.
type AuthRequiredHandler = () => void;
let authRequiredHandler: AuthRequiredHandler | null = null;

export function setAuthRequiredHandler(fn: AuthRequiredHandler | null): void {
  authRequiredHandler = fn;
}

/**
 * The data-plane credential headers every agent request carries: the dashboard
 * session minted by the PIN gate, and the API key from the Mission Control
 * deep link. A paired agent requires one of them off-box; on-box neither is
 * needed and none is sent when none is stored.
 */
export function credentialHeaders(): Record<string, string> {
  const headers: Record<string, string> = {};
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const storedKey = getApiKey();
  if (storedKey) headers["X-ADOS-Key"] = storedKey;
  return headers;
}

export async function apiFetch<T = unknown>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    ...credentialHeaders(),
  };

  const init: RequestInit = {
    method: opts.method ?? "GET",
    headers,
    signal: opts.signal,
  };

  if (opts.body instanceof FormData) {
    // The browser sets the multipart boundary itself.
    init.body = opts.body;
  } else if (opts.body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }

  const res = await fetch(path, init);

  // The agent refused: hand the question to the access gate, whose probe tells
  // a dead session (PIN splash) from a refusal of this one request (stay put).
  // The gate's own probe passes `skipAuthSignal` so it does not recurse.
  if (isAuthChallenge(res.status) && !opts.signal?.aborted && !opts.skipAuthSignal) {
    authRequiredHandler?.();
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
