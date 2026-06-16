// Thin wrapper around fetch for the agent's REST surface. All paths are
// relative ("/api/...") so the same code works behind Vite's dev proxy
// and against the real agent at runtime. JSON in, JSON out, throws on
// non-2xx with a useful message.

import { getApiKey, setApiKey } from "./api-key";

export class ApiError extends Error {
  status: number;
  body: unknown;

  constructor(message: string, status: number, body: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

interface FetchOptions {
  method?: "GET" | "POST" | "PUT" | "DELETE";
  body?: unknown;
  signal?: AbortSignal;
  // Internal: set once a request has already retried after a key prompt, so a
  // still-401 response (wrong key / unpaired) does not loop.
  retriedAfterKeyPrompt?: boolean;
}

// A paired agent requires the API key on its data routes. The dashboard reads
// it from localStorage; on a direct LAN visit it is empty and every panel fetch
// 401s. Rather than make the operator SSH in and grab the key, the agent's own
// webapp is first-party and reachable over the LAN — which is the local-first
// pairing trust boundary — so it acquires the key by CLAIMING locally. The
// claim is idempotent on an already-paired agent (it returns the live key and
// never rotates), so this never disturbs the GCS or other clients. Only if the
// local claim fails do we fall back to a manual key prompt. Single-flight so a
// burst of concurrent 401s triggers exactly one acquisition.
let keyAcquireInFlight: Promise<boolean> | null = null;

function acquireApiKey(): Promise<boolean> {
  if (!keyAcquireInFlight) {
    keyAcquireInFlight = (async () => {
      // Local claim: the agent serves this webapp, so a same-origin POST to its
      // public pairing-claim endpoint returns the current key with no SSH.
      try {
        const resp = await fetch("/api/pairing/claim", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ user_id: "ados-dashboard" }),
        });
        if (resp.ok) {
          const data = (await resp.json()) as { api_key?: string };
          const key = data.api_key?.trim();
          if (key) {
            setApiKey(key);
            return true;
          }
        }
      } catch {
        // Network/route failure — fall through to the manual prompt.
      }
      // Fallback only: claim unavailable. Ask for the key, or point at the
      // Mission Control path that passes it automatically (?ados_key=).
      const entered = window.prompt(
        "This agent is paired. Enter its API key (shown by `ados status`),\n" +
          "or open this dashboard from Mission Control to connect automatically.",
      );
      const key = entered?.trim();
      if (key) {
        setApiKey(key);
        return true;
      }
      return false;
    })().finally(() => {
      // Release the single-flight latch on the next tick so a later 401 (e.g. a
      // wrong key) can retry acquisition.
      setTimeout(() => {
        keyAcquireInFlight = null;
      }, 0);
    });
  }
  return keyAcquireInFlight;
}

export async function apiFetch<T = unknown>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };

  // A paired agent requires the key on its data routes. Send the stored key
  // when present; a 401 below drives an in-band key-entry prompt + retry so a
  // directly-visited dashboard is not left blank.
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

  // Direct-visit auth: a 401 on a paired agent means we have no (or a stale)
  // key. Acquire it (claim locally over the LAN, prompt only as a fallback) and
  // retry the request a single time.
  if (res.status === 401 && !opts.retriedAfterKeyPrompt && !opts.signal?.aborted) {
    const acquired = await acquireApiKey();
    if (acquired) {
      return apiFetch<T>(path, { ...opts, retriedAfterKeyPrompt: true });
    }
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
