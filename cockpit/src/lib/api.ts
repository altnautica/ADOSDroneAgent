// Typed helpers over the agent REST surface on the same origin (:8080). The
// transport (credentials, errors) is the shared `apiFetch`.

import { apiFetch } from "@/shared/api-fetch";
import type {
  AgentConfig,
  BatteryHealth,
  GsStatus,
  LinkView,
  RosterCamera,
  StatusFull,
  VehicleState,
} from "@/lib/types";

/** The composite ground-station status snapshot (`/api/v1/ground-station/status`).
 *  The status strip and the Link/Mesh/Uplink screens read this so they never
 *  drift from the OLED. */
export function getGsStatus(signal?: AbortSignal): Promise<GsStatus> {
  return apiFetch<GsStatus>("/api/v1/ground-station/status", { signal });
}

function asRecord(v: unknown): Record<string, unknown> {
  return v && typeof v === "object" ? (v as Record<string, unknown>) : {};
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

/** The radio-link diagnosis a drone's `/api/wfb` carries that the consolidated
 *  status body does not (`link_diag` and its two backing counters). */
export interface LinkDiagnosis {
  link_diag: string | null;
  packets_all: number | null;
  decrypt_errors: number | null;
  packets_received: number | null;
}

export async function getLinkDiagnosis(signal?: AbortSignal): Promise<LinkDiagnosis> {
  const wfb = asRecord(await apiFetch<unknown>("/api/wfb", { signal }));
  return {
    link_diag: str(wfb.link_diag),
    packets_all: num(wfb.packets_all),
    decrypt_errors: num(wfb.decrypt_errors),
    packets_received: num(wfb.packets_received),
  };
}

/**
 * Map a drone's consolidated `/api/status/full` body (plus the last radio
 * diagnosis) onto the `GsStatus` shape the shell reads, so the status strip
 * and Feed work on both profiles. Fields the drone does not report stay null;
 * the ground-station-only blocks (mesh, uplink) stay empty because their
 * screens are hidden on a drone. Never fabricates: no recording claim, no PIC.
 */
export function droneStatusFromFull(full: StatusFull, diag: LinkDiagnosis | null): GsStatus {
  const health = asRecord(full.health);
  const resources = asRecord(full.resources);
  const radio = asRecord(full.radio);
  const video = asRecord(full.video);
  const telem = asRecord(full.telemetry) as VehicleState;
  const radioPaired = radio.paired === true;

  const link: LinkView = {
    rssi_dbm: num(radio.rssiDbm),
    bitrate_kbps: num(radio.bitrateKbps),
    fec_recovered: num(radio.fecRecovered) ?? 0,
    fec_failed: num(radio.fecLost) ?? 0,
    channel: num(radio.channel),
    snr_db: num(radio.snrDb),
    noise_dbm: num(radio.noiseDbm),
    packets_received: diag?.packets_received ?? 0,
    packets_lost: num(radio.packetsLost) ?? 0,
    loss_percent: num(radio.lossPercent),
    tx_power_dbm: num(radio.txPowerDbm),
    state: str(radio.state) ?? "unknown",
    link_diag: diag?.link_diag ?? null,
    packets_all: diag?.packets_all ?? null,
    decrypt_errors: diag?.decrypt_errors ?? null,
  };

  return {
    profile: "drone",
    fc_connected: full.fc_connected === true,
    perception_tier: str(full.perceptionTier),
    heartbeat_age_s: num(full.heartbeatAgeS),
    paired_drone: {
      device_id: radioPaired ? str(radio.pairedWithDeviceId) : null,
      key_fingerprint: str(radio.publicKeyFingerprint),
      fc_mode: telem.mode ?? null,
      battery_pct: num(telem.battery?.remaining),
      gps_sats: num(telem.gps?.satellites),
    },
    link,
    gcs: null,
    network: {
      ap_ssid: null,
      ap_ip: null,
      usb_ip: null,
      uplink_type: null,
      uplink_reachable: null,
    },
    system: {
      cpu_pct: num(health.cpu_percent) ?? num(resources.cpu_percent),
      ram_used_mb: num(resources.memory_used_mb),
      ram_total_mb: num(resources.memory_total_mb),
      temp_c: num(health.temperature) ?? num(resources.temperature),
      uptime_seconds: num(full.uptime_seconds),
      agent_version: str(full.version),
    },
    recording: video.recording === true,
    video: {
      recording: video.recording === true,
      recording_filename: str(video.recording_filename),
      recording_started_at: str(video.recording_started_at),
      state: str(video.state),
      streams: Array.isArray(video.streams) ? (video.streams as GsStatus["video"]["streams"]) : [],
    },
    role: { current: "", configured: "", supported: [], mesh_capable: false },
    mesh: { up: false, peer_count: 0, selected_gateway: null, partition: false, mesh_id: null },
  };
}

export function getStatusFull(signal?: AbortSignal): Promise<StatusFull> {
  return apiFetch<StatusFull>("/api/status/full", { signal });
}

/** The whole sanitized config tree (`GET /api/config`), the Settings source. */
export function getConfig(signal?: AbortSignal): Promise<AgentConfig> {
  return apiFetch<AgentConfig>("/api/config", { signal });
}

/** The live vehicle state (`GET /api/telemetry`), `{}` when none is heard. */
export function getTelemetry(signal?: AbortSignal): Promise<VehicleState> {
  return apiFetch<VehicleState>("/api/telemetry", { signal });
}

/** The battery engine's per-pack read model (`GET /api/v1/battery`). */
export function getBatteryHealth(signal?: AbortSignal): Promise<BatteryHealth> {
  return apiFetch<BatteryHealth>("/api/v1/battery", { signal });
}

/** The agent's capability flags (`GET /api/version`). */
export async function getCapabilities(signal?: AbortSignal): Promise<string[]> {
  const v = asRecord(await apiFetch<unknown>("/api/version", { signal }));
  return Array.isArray(v.capabilities)
    ? v.capabilities.filter((c): c is string => typeof c === "string")
    : [];
}

/** The reconciled camera roster (`GET /api/video/roster`). */
export function getRoster(signal?: AbortSignal): Promise<{ cameras: RosterCamera[] }> {
  return apiFetch<{ cameras: RosterCamera[] }>("/api/video/roster", { signal });
}

/** The FC's `COMMAND_ACK` outcome the command route correlates. `observed:false`
 *  means sent but not acknowledged (never a fabricated success). */
export interface CommandAck {
  observed: boolean;
  result?: number;
  result_name?: string;
  accepted?: boolean;
  statustext?: string;
}

export interface CommandResponse {
  status?: string;
  cmd?: string;
  mode?: string;
  altitude?: number;
  ack?: CommandAck;
}

/** The commands `POST /api/command` accepts from this cockpit. */
export type FlightCommand =
  | "arm"
  | "disarm"
  | "takeoff"
  | "land"
  | "rtl"
  | "mode"
  | "killswitch"
  | "pausemission"
  | "resumemission";

/** Send one high-level flight command (`POST /api/command`). Throws `ApiError`
 *  on a 503 (no FC link) or 400 (unknown command / bad argument). */
export function sendCommand(
  cmd: FlightCommand,
  args: (string | number)[] = [],
  signal?: AbortSignal,
): Promise<CommandResponse> {
  return apiFetch<CommandResponse>("/api/command", {
    method: "POST",
    body: { cmd, args },
    signal,
  });
}

/** Start the ground-station video recorder. */
export function startRecording(signal?: AbortSignal): Promise<unknown> {
  return apiFetch("/api/v1/ground-station/recording/start", { method: "POST", body: {}, signal });
}

/** Stop the in-flight recording. */
export function stopRecording(signal?: AbortSignal): Promise<unknown> {
  return apiFetch("/api/v1/ground-station/recording/stop", { method: "POST", body: {}, signal });
}
