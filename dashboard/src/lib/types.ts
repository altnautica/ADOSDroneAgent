// TypeScript types for the agent's REST responses we consume in the
// dashboard. These shapes mirror the FastAPI Pydantic models on the
// agent side; we keep them defensive (most fields optional) because
// the agent ships nulls + empty objects whenever the underlying
// service hasn't reported yet.

export type Profile = "drone" | "ground_station" | "auto" | "unknown";
export type GroundRole = "direct" | "relay" | "receiver";
export type Severity = "ok" | "warn" | "err" | "info" | "idle";

// Operating-region / RF regulatory posture.
//   "unrestricted" — the radio brings up and transmits on the
//     configured channel without enforcing a jurisdiction. The
//     operator is responsible for local RF compliance.
//   "region" — pin a single operating region (ISO 3166-1 alpha-2)
//     and re-enable the strict regulatory gate + power clamp.
export type RegulatoryMode = "unrestricted" | "region";

// /api/v1/setup/status -> network.regulatory (additive; absent on
// older agents -> treat as unrestricted).
export interface RegulatoryInfo {
  mode?: RegulatoryMode;
  region?: string | null;
  ack_operator?: string | null;
  ack_at?: string | null;
}

// /api/v1/setup/status
export interface SetupStatus {
  version: string;
  device_id: string;
  device_name?: string;
  profile: Profile;
  ground_role?: GroundRole;
  setup_complete: boolean;
  setup_finalized: boolean;
  setup_skipped?: boolean;
  setup_state?: string;
  profile_source?: string;
  profile_suggestion?: ProfileSuggestion;
  completion_percent: number;
  next_action?: string;
  steps?: SetupStep[];
  cloud_choice?: CloudChoice;
  network?: NetworkInfo;
  services?: ServicesInfo;
  mavlink?: MavlinkInfo;
  video?: VideoInfo;
  hardware_check?: HardwareCheck;
  remote_access?: RemoteAccess;
  access_urls?: SetupAccessUrl[];
  regulatory?: RegulatoryInfo;
  // LAN-routable host the agent derived for clients elsewhere on the LAN.
  // Prefers the resolvable system hostname, then the mDNS host, then an IP.
  // Empty when no LAN identity could be derived.
  lan_host?: string;
}

export interface ProfileSuggestion {
  detected: Profile;
  source: string;
  ground_role_hint?: GroundRole;
  ground_score?: number;
  air_score?: number;
  mesh_capable?: boolean;
  signals?: Record<string, unknown>;
  confirmed?: boolean;
  detected_at?: string;
}

export interface SetupStep {
  id: string;
  label: string;
  state: "complete" | "needs_action" | "in_progress" | "optional" | "skipped";
  detail?: string;
  action_label?: string;
  href?: string;
}

export interface CloudChoice {
  mode?: "cloud" | "self_hosted" | "local";
  backend_url?: string;
  mqtt_broker?: string;
  mqtt_port?: number;
}

export interface NetworkInfo {
  // System hostname (may be empty). The mDNS reach name is `mdns_host`, which
  // the agent only populates with the name avahi actually publishes.
  hostname?: string;
  mdns_host?: string;
  local_ips?: string[];
  wifi_ssid?: string;
  hotspot_enabled?: boolean;
  // The name the setup AP actually broadcasts, resolved by the agent from the
  // configured `network.hotspot.ssid` with the device id substituted in. Absent
  // on an older agent, in which case the name is unknown, not guessable.
  hotspot_ssid?: string;
  uplink_kind?: string;
  rssi_dbm?: number | null;
  ip_addresses?: Record<string, string>;
}

export interface ServicesInfo {
  by_name?: Record<string, ServiceState>;
}

export interface ServiceState {
  active: boolean;
  state: string;
  sub_state?: string;
  pid?: number | null;
}

export interface MavlinkInfo {
  port?: string;
  baud?: number;
  connected?: boolean;
}

export interface VideoInfo {
  state?: string;
  whep_url?: string;
  hls_url?: string;
  bitrate_kbps?: number;
}

// Mirrors `HardwareCheckItem` on the agent (setup/models.py).
// One row per probed component. `state` order of severity:
//   "ok" < "warning" < "missing" < "checking" < "unknown".
// `required=true` items count toward setup completion; the rest
// are optional add-ons (radios, modem, GPS, etc.).
export type HardwareItemState =
  | "ok"
  | "warning"
  | "missing"
  | "checking"
  | "unknown";

export interface HardwareItem {
  id: string;
  label: string;
  required: boolean;
  state: HardwareItemState;
  detail?: string;
  fix_hint?: string;
}

export interface HardwareCheck {
  profile?: string;
  ground_role?: string;
  items?: HardwareItem[];
  last_run?: string;
}

export interface RemoteAccess {
  cloudflare_state?: string;
  hostname?: string;
}

// One node-advertised reach entry. The agent only lists names/URLs it has
// derived as actually reachable (the mDNS entry uses the avahi-published
// hostname, never a constructed one), so a consumer renders these verbatim.
export interface SetupAccessUrl {
  kind: "setup" | "api" | "mission_control" | "video" | "mavlink" | "cloud";
  label: string;
  url: string;
  source: "local" | "hotspot" | "usb" | "mdns" | "cloud" | "configured";
  primary?: boolean;
  id?: string;
  role?: string;
  codec?: string;
}

// /api/v1/dashboard/snapshot (api/routes/dashboard.py). Only these slices are
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
  channel: number;
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
