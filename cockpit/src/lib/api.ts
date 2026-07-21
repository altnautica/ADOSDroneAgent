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
  LinkView,
  NetworkView,
  PairedDrone,
  RosterCamera,
  SystemView,
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

// ── drone-profile status composition ───────────────────────────────────────
// A drone has no single composite status endpoint (that is a ground-station
// concept). Its equivalent is composed from the endpoints it does serve:
// /api/status (system health + version + uptime), /api/wfb (the radio link),
// /api/telemetry (the FC vehicle state), and /api/pairing/info (identity +
// radio-pair state). We map those into the same GsStatus shape the shell reads
// so the status strip + Feed HUD work unchanged, with the ground-station-only
// blocks (mesh, uplink) left empty (their screens are hidden on a drone).

function asRecord(v: unknown): Record<string, unknown> {
  return v && typeof v === "object" ? (v as Record<string, unknown>) : {};
}
function numOr(v: unknown, fallback: number | null): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
}
function strOr(v: unknown, fallback: string | null): string | null {
  return typeof v === "string" ? v : fallback;
}

/** Compose a `GsStatus`-shaped snapshot for a DRONE from its own endpoints.
 *  `/api/status` is the anchor (a failure throws so the caller flips `stale`
 *  and keeps the last snapshot); the radio, telemetry, and pairing reads are
 *  best-effort — a missing one degrades a field to a dash, never the whole
 *  snapshot (Rule 44 — a status surface never fabricates). */
export async function getDroneStatus(signal?: AbortSignal): Promise<GsStatus> {
  const status = asRecord(
    await apiFetch<Record<string, unknown>>("/api/status", { signal }),
  );
  const [wfbR, telemR, pairR] = await Promise.allSettled([
    apiFetch<Record<string, unknown>>("/api/wfb", { signal }),
    apiFetch<VehicleState>("/api/telemetry", { signal }),
    apiFetch<Record<string, unknown>>("/api/pairing/info", { signal }),
  ]);
  const wfb = asRecord(wfbR.status === "fulfilled" ? wfbR.value : {});
  const telem: VehicleState = telemR.status === "fulfilled" ? telemR.value : {};
  const pair = asRecord(pairR.status === "fulfilled" ? pairR.value : {});

  const health = asRecord(status.health);
  const system: SystemView = {
    cpu_pct: numOr(health.cpu_percent, null),
    // /api/status reports memory_percent, not MB, so the MB fields stay null
    // rather than fabricating a figure.
    ram_used_mb: null,
    ram_total_mb: null,
    temp_c: numOr(health.temperature, null),
    uptime_seconds: numOr(status.uptime_seconds, null),
    agent_version: strOr(status.version, null),
  };

  // The drone's OWN flight state populates paired_drone so the Feed HUD shows
  // this drone's telemetry (on a ground station this block is the RECEIVED
  // drone). device_id reflects radio-pair state so the strip's linked/none is
  // honest — only "linked" once the radio is bound.
  const radioPaired = pair.radio_paired === true;
  const pairedDrone: PairedDrone = {
    device_id: radioPaired ? strOr(pair.device_id, null) : null,
    key_fingerprint: strOr(pair.key_fingerprint, null),
    fc_mode: telem.mode ?? null,
    battery_pct: telem.battery?.remaining ?? null,
    gps_sats: telem.gps?.satellites ?? null,
  };

  const link: LinkView = {
    rssi_dbm: numOr(wfb.rssi_dbm, null),
    bitrate_kbps: numOr(wfb.bitrate_kbps, null),
    fec_recovered: numOr(wfb.fec_recovered, 0) ?? 0,
    fec_failed: numOr(wfb.fec_failed, 0) ?? 0,
    channel: numOr(wfb.channel, null),
    snr_db: numOr(wfb.snr_db, null),
    noise_dbm: numOr(wfb.noise_dbm, null),
    packets_received: numOr(wfb.packets_received, 0) ?? 0,
    packets_lost: numOr(wfb.packets_lost, 0) ?? 0,
    loss_percent: numOr(wfb.loss_percent, null),
    tx_power_dbm: numOr(wfb.tx_power_dbm, null),
    state: strOr(wfb.link_state, null) ?? strOr(wfb.state, null) ?? "unknown",
  };

  const network: NetworkView = {
    ap_ssid: null,
    ap_ip: null,
    usb_ip: null,
    uplink_type: null,
    uplink_reachable: false,
  };

  return {
    profile: "drone",
    paired_drone: pairedDrone,
    link,
    gcs: { clients: [], pic_id: null },
    network,
    system,
    recording: false,
    video: { recording: false, recording_filename: null },
    // No mesh role on a drone; the Mesh screen is hidden on this profile.
    role: { current: "", configured: "", supported: [], mesh_capable: false },
    mesh: {
      up: false,
      peer_count: 0,
      selected_gateway: null,
      partition: false,
      mesh_id: null,
    },
  };
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
