// TypeScript types for the agent REST responses the dashboard consumes that
// have no generated type yet. The setup-status types are generated from the
// agent's response schemas into `@/shared/agent-api.generated`.

export type Profile =
  | "drone"
  | "ground_station"
  | "workstation"
  | "compute"
  | "visor"
  | "auto"
  | "unknown";
export type GroundRole = "direct" | "relay" | "receiver";
export type { Severity } from "@/shared/format";

// Operating-region / RF regulatory posture.
//   "unrestricted" — the radio brings up and transmits on the
//     configured channel without enforcing a jurisdiction. The
//     operator is responsible for local RF compliance.
//   "region" — pin a single operating region (ISO 3166-1 alpha-2)
//     and re-enable the strict regulatory gate + power clamp.
export type RegulatoryMode = "unrestricted" | "region";

// GET /api/config -> network.regulatory (absent -> treat as unrestricted).
// Setup status does not carry this block.
export interface RegulatoryInfo {
  mode?: RegulatoryMode;
  region?: string | null;
  ack_operator?: string | null;
  ack_at?: string | null;
}

// /api/v1/dashboard/snapshot (ados-control routes/dashboard.rs). Only these slices are
// sent; everything the snapshot does not measure lives on its own route.
export interface DashboardSnapshot {
  video: VideoSnapshot;
  fc: FcSnapshot;
  cloud: CloudSnapshot;
}

export interface VideoSnapshot {
  // Configured encoder settings (0 when unset).
  codec?: string;
  width?: number;
  height?: number;
  fps?: number;
  target_bitrate_kbps?: number | null;
  state: "running" | "ready" | "no_camera";
  /** Measured off the media server's byte counter; null until two readings of
   *  a live stream exist, never the configured target. */
  bitrate_kbps: number | null;
  glass_to_glass_ms: number | null;
}

export interface FcSnapshot {
  vehicle: string | null;
  vehicle_id?: number | null;
  firmware: string | null;
  firmware_id?: number | null;
  mode: string | null;
  /** Only what the heartbeat said: null until the FC has reported it. */
  armed: boolean | null;
  gps: { fix_type: number | null; satellites_visible: number | null; hdop: number | null };
  battery: { voltage: number | null; remaining: number | null };
  rc: number | null;
  fc_port: string | null;
  fc_baud: number | null;
  connected: boolean;
  last_heartbeat: string | null;
}

/** The cloud posture from config plus the live pairing code. The relay's own
 *  link state is not part of the snapshot. */
export interface CloudSnapshot {
  mode: string | null;
  drone_id: string;
  pairing_code: string;
}

// /api/status (lightweight heartbeat used by the placeholder + sanity)
export interface AgentHeartbeat {
  version: string;
  uptime_seconds: number;
  board: { name?: string; tier?: number; ram_mb?: number };
  health: {
    cpu_percent: number;
    memory_percent: number;
    disk_percent: number;
    temperature: number | null;
  };
  fc_connected: boolean;
  fc_port?: string;
  fc_baud?: number;
  /** Canonical FC firmware family: "ardupilot" | "px4" | "betaflight" | "inav" | "unknown". */
  fcFirmware?: string;
  /** Detected FC firmware variant for MSP FCs: "betaflight" | "inav". */
  fcVariant?: string;
  /** Short link hint: "none" | "no_heartbeat" | "msp_detected". */
  fcLinkHint?: string;
  /** Honest FC reachability: MAVLink alive, or a serial FC identified over MSP.
   *  An MSP FC (Betaflight/iNav) never emits a MAVLink heartbeat, so `fc_connected`
   *  stays false while `fcReachable` is true. */
  fcReachable?: boolean;
}

// /api/v1/network/client/* — Wi-Fi client surface
export interface WifiNetwork {
  ssid: string;
  bssid: string;
  signal: number;
  security: string;
  in_use: boolean;
}

export interface WifiSavedConnection {
  name: string;
  type: string;
  device: string | null;
  autoconnect: boolean;
}

export interface WifiStatus {
  connected: boolean;
  ssid: string | null;
  bssid: string;
  signal: number | null;
  ip: string | null;
  gateway: string | null;
  security: string | null;
}

export interface WifiJoinResult {
  joined: boolean;
  ip: string | null;
  gateway: string | null;
  error: string | null;
}

export interface WifiLeaveResult {
  left: boolean;
  previous_ssid: string | null;
}

export interface WifiForgetResult {
  forgot: boolean;
  name: string;
  error: string | null;
}

// /api/wfb — live WFB-tx / WFB-rx runtime state
export interface WfbStatus {
  state: string;
  interface: string;
  /** The configured home channel. */
  channel: number;
  /** The live interface channel; null until the radio reports one. */
  actual_channel: number | null;
  frequency_mhz: number | null;
  bandwidth_mhz: number | null;
  adapter: {
    driver: string;
    chipset: string;
    supports_monitor: boolean;
  };
  rssi_dbm: number | null;
  noise_dbm: number | null;
  snr_db: number | null;
  packets_received: number;
  packets_lost: number;
  loss_percent: number;
  fec_recovered: number;
  fec_failed: number;
  bitrate_kbps: number;
  restart_count: number;
  samples: number;
  tx_power_dbm: number;
  tx_power_max_dbm: number;
  topology: string;
  mcs_index: number;
  regulatory_domain: string;
  rssi_min?: number | null;
  rssi_max?: number | null;
  profile?: string;
  bitrate_mbps?: number;
}
