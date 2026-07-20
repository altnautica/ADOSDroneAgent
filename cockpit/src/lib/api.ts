// Thin client over the agent REST surface on the same origin (:8080). All
// paths are absolute ("/api/...", "/whep") so the same code works behind Vite's
// dev proxy and against the real agent at runtime. On-box the panel is trusted;
// when reached off-box the optional session/key credentials are attached.

import { getApiKey } from "@/lib/api-key";
import { getSession } from "@/lib/session";
import type { AgentConfig, ConfigValue, GsStatus } from "@/lib/types";

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
    headers["Content-Type"] = "application/json";
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

// ── typed helpers the screens consume ──────────────────────────────────────

/** The composite ground-station status snapshot (the status strip + the
 *  Link/Mesh/Uplink screens read this so they never drift from the OLED). */
export function getGsStatus(signal?: AbortSignal): Promise<GsStatus> {
  return apiFetch<GsStatus>("/api/v1/ground-station/status", { signal });
}

/** The whole sanitized config tree (`GET /api/config`), the Settings source. */
export function getConfig(signal?: AbortSignal): Promise<AgentConfig> {
  return apiFetch<AgentConfig>("/api/config", { signal });
}

/** Write one dot-path config leaf (`PUT /api/config {key, value}`). The value
 *  is sent as a string; the leaf's Pydantic type cast interprets it. */
export function putConfig(
  key: string,
  value: string,
  signal?: AbortSignal,
): Promise<AgentConfig> {
  return apiFetch<AgentConfig>("/api/config", {
    method: "PUT",
    body: { key, value },
    signal,
  });
}

/** The radio link view (`GET /api/wfb`), the Link screen's tuning source. */
export function getWfb<T = Record<string, ConfigValue>>(
  signal?: AbortSignal,
): Promise<T> {
  return apiFetch<T>("/api/wfb", { signal });
}
