// Thin wrapper around fetch for the agent's REST surface. All paths are
// relative ("/api/...") so the same code works behind Vite's dev proxy
// and against the real agent at runtime. JSON in, JSON out, throws on
// non-2xx with a useful message.

import { getApiKey } from "./api-key";

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
}

export async function apiFetch<T = unknown>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };

  // Same-origin browser requests are accepted by the agent without an
  // explicit key. The stored key is consulted only when the dashboard
  // runs behind a tunnel / reverse proxy where the Origin no longer
  // matches the agent's listener.
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
