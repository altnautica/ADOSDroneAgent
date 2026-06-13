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
// 401s. Prompt the operator for the key once (the value `ados status` prints),
// store it, and let the in-flight requests retry. Single-flight so a burst of
// concurrent 401s raises exactly one prompt.
let keyPromptInFlight: Promise<boolean> | null = null;

function promptForApiKey(): Promise<boolean> {
  if (!keyPromptInFlight) {
    keyPromptInFlight = (async () => {
      const entered = window.prompt(
        "This agent is paired and requires its API key.\n" +
          "Run `ados status` on the agent and paste the key here.",
      );
      const key = entered?.trim();
      if (key) {
        setApiKey(key);
        return true;
      }
      return false;
    })().finally(() => {
      // Release the single-flight latch on the next tick so a later 401 (e.g. a
      // wrong key) can prompt again.
      setTimeout(() => {
        keyPromptInFlight = null;
      }, 0);
    });
  }
  return keyPromptInFlight;
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
  // key. Prompt once and retry the request a single time with the entered key.
  if (res.status === 401 && !opts.retriedAfterKeyPrompt && !opts.signal?.aborted) {
    const entered = await promptForApiKey();
    if (entered) {
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
