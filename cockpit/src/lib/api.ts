// Thin client over the agent REST surface on the same origin (:8080). All
// paths are absolute ("/api/...", "/whep") so the same code works behind Vite's
// dev proxy and against the real agent at runtime. On-box the panel is trusted;
// when reached off-box the optional session/key credentials are attached.

import { getApiKey } from "@/lib/api-key";
import { getSession } from "@/lib/session";
import type {
  AgentConfig,
  ConfigValue,
  GsStatus,
  RosterCamera,
  VehicleState,
} from "@/lib/types";

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

/** The whole sanitized config tree (`GET /api/config`), the Settings source.
 *  The Settings tree writes one leaf at a time via `PUT /api/config {key,value}`,
 *  which is owned by the config store (it needs the typed error/persisted echo,
 *  not the config shape the GET returns). */
export function getConfig(signal?: AbortSignal): Promise<AgentConfig> {
  return apiFetch<AgentConfig>("/api/config", { signal });
}

/** The radio link view (`GET /api/wfb`), the Link screen's tuning source. */
export function getWfb<T = Record<string, ConfigValue>>(
  signal?: AbortSignal,
): Promise<T> {
  return apiFetch<T>("/api/wfb", { signal });
}

/** The paired drone's live vehicle state (`GET /api/telemetry`) — attitude,
 *  position, velocity, battery, GPS — the Feed HUD's flight-instrument source.
 *  Returns `{}` when no vehicle has been heard. */
export function getTelemetry(signal?: AbortSignal): Promise<VehicleState> {
  return apiFetch<VehicleState>("/api/telemetry", { signal });
}

/** The reconciled camera roster (`GET /api/video/roster`). The Feed shows
 *  multi-stream tabs only when more than one camera is reported. A ground
 *  station returns an empty list. */
export function getRoster(
  signal?: AbortSignal,
): Promise<{ cameras: RosterCamera[] }> {
  return apiFetch<{ cameras: RosterCamera[] }>("/api/video/roster", { signal });
}

/** Start the ground-station video recorder
 *  (`POST /api/v1/ground-station/recording/start`). */
export function startRecording(signal?: AbortSignal): Promise<unknown> {
  return apiFetch("/api/v1/ground-station/recording/start", {
    method: "POST",
    body: {},
    signal,
  });
}

/** Stop the in-flight recording (`POST /api/v1/ground-station/recording/stop`). */
export function stopRecording(signal?: AbortSignal): Promise<unknown> {
  return apiFetch("/api/v1/ground-station/recording/stop", {
    method: "POST",
    body: {},
    signal,
  });
}
