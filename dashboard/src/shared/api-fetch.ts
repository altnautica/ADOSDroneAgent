// Thin JSON client over the agent REST surface on the same origin (:8080).
// Paths are absolute ("/api/...") so the same code works behind the Vite dev
// proxy and against the real agent. Throws `ApiError` on any non-2xx.

import { getApiKey } from "./api-key";
import { clearSession, getSession } from "./session";

export class ApiError extends Error {
  status: number;
  body: unknown;

  constructor(message: string, status: number, body: unknown) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }

  /** The agent's machine-readable error code when the body carries one
   *  (`{"error":"E_ARMED"}`, `{"detail":{"error":{"code":…}}}`), else null. */
  get code(): string | null {
    return errorCode(this.body);
  }
}

function errorCode(body: unknown): string | null {
  if (!body || typeof body !== "object") return null;
  const b = body as Record<string, unknown>;
  if (typeof b.error === "string") return b.error;
  if (b.error && typeof b.error === "object") {
    const c = (b.error as Record<string, unknown>).code;
    if (typeof c === "string") return c;
  }
  if (b.detail && typeof b.detail === "object") return errorCode(b.detail);
  return null;
}

/** A readable message from an agent error body (`{"detail": "..."}`,
 *  `{"detail": {"error": {"message": "..."}}}`), else the fallback. */
export function errorDetail(body: unknown, fallback: string): string {
  if (!body || typeof body !== "object") {
    return typeof body === "string" && body ? body : fallback;
  }
  const b = body as Record<string, unknown>;
  if (typeof b.detail === "string") return b.detail;
  if (typeof b.message === "string") return b.message;
  if (b.detail && typeof b.detail === "object") return errorDetail(b.detail, fallback);
  if (b.error && typeof b.error === "object") {
    const m = (b.error as Record<string, unknown>).message;
    if (typeof m === "string") return m;
  }
  if (typeof b.error === "string") return b.error;
  return fallback;
}

/**
 * Whether a failed response means "not authorized yet" rather than "broken".
 * 401: a paired node reached off-box without a valid credential. 403: an
 * unpaired node asking for the PIN (or a refusal of this one request, which is
 * why a 403 alone never drops the stored session).
 */
export function isAuthChallenge(status: number): boolean {
  return status === 401 || status === 403;
}

/** The data-plane credential headers: the PIN-minted session and the stored
 *  API key, whichever are present. On-box neither is needed. */
export function credentialHeaders(): Record<string, string> {
  const headers: Record<string, string> = {};
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const key = getApiKey();
  if (key) headers["X-ADOS-Key"] = key;
  return headers;
}

type AuthChallengeHandler = (status: number) => void;
let authChallengeHandler: AuthChallengeHandler | null = null;

/** Register the app's access gate. Called on a 401/403 unless the request set
 *  `skipAuthSignal` (the gate's own probe, so it does not recurse). */
export function setAuthChallengeHandler(fn: AuthChallengeHandler | null): void {
  authChallengeHandler = fn;
}

export interface FetchOptions {
  method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
  /** JSON-encoded, except a FormData body, which is sent as multipart. */
  body?: unknown;
  signal?: AbortSignal;
  skipAuthSignal?: boolean;
}

export async function apiFetch<T = unknown>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const creds = credentialHeaders();
  const headers: Record<string, string> = { Accept: "application/json", ...creds };
  const init: RequestInit = {
    method: opts.method ?? "GET",
    headers,
    signal: opts.signal,
  };

  if (typeof FormData !== "undefined" && opts.body instanceof FormData) {
    init.body = opts.body;
  } else if (opts.body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }

  const res = await fetch(path, init);

  // Either credential suffices, so a 401 means a session we sent was not
  // accepted (expired or revoked by a PIN reset): drop it.
  if (res.status === 401 && creds["X-ADOS-Dashboard-Session"]) clearSession();
  if (isAuthChallenge(res.status) && !opts.signal?.aborted && !opts.skipAuthSignal) {
    authChallengeHandler?.(res.status);
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
    throw new ApiError(`${res.status} ${errorDetail(body, res.statusText)}`, res.status, body);
  }
  return body as T;
}
