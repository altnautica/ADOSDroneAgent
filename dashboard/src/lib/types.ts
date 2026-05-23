// TypeScript types for the agent's REST responses we consume in the
// dashboard. These shapes mirror the FastAPI Pydantic models on the
// agent side; we keep them defensive (most fields optional) because
// the agent ships nulls + empty objects whenever the underlying
// service hasn't reported yet.

export type Profile = "drone" | "ground_station" | "auto" | "unknown";
export type GroundRole = "direct" | "relay" | "receiver";
export type Severity = "ok" | "warn" | "err" | "info" | "idle";

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
  access_urls?: AccessUrls;
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
  wifi_ssid?: string;
  hotspot_enabled?: boolean;
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

export interface AccessUrls {
  setup?: string;
  dashboard?: string;
}

// /api/v1/dashboard/snapshot
export interface DashboardSnapshot {
  video: VideoSnapshot;
  fc: FcSnapshot;
  mavlink_rates: Record<string, number>;
  camera: CameraSnapshot;
  sensors: SensorEntry[];
  plugins: PluginEntry[];
  cloud: CloudSnapshot;
  network: NetworkSnapshot;
  wfb_rx: WfbRxSnapshot;
  mesh: MeshSnapshot;
  sources: SourcesSnapshot;
  display: Record<string, unknown>;
  oled: Record<string, unknown>;
  buttons: ButtonsSnapshot;
  joystick: Record<string, unknown>;
}

export interface VideoSnapshot {
  codec: string;
  width: number;
  height: number;
  fps: number;
  bitrate_kbps: number;
  state: string;
  glass_to_glass_ms: number | null;
}

export interface FcSnapshot {
  vehicle: string | null;
  firmware: string | null;
  mode: string | null;
  armed: boolean;
  gps: { fix_type: number | null; satellites_visible: number | null; hdop: number | null };
  battery: { voltage: number | null; remaining: number | null };
  link_quality: number | null;
  rc: number | null;
  prearm: string | null;
  fc_port: string;
  fc_baud: number;
  connected: boolean;
  last_heartbeat: string | null;
}

export interface CameraSnapshot {
  device: string;
  codec: string;
  width: number;
  height: number;
  fps: number;
  bitrate_kbps: number;
  encoder_api: string;
  state: string;
  dropped_frames: number | null;
  encoder_cpu_pct: number | null;
}

export interface SensorEntry {
  id: string;
  name?: string;
  state?: string;
  value?: unknown;
}

export interface PluginEntry {
  id: string;
  name?: string;
  enabled?: boolean;
  state?: string;
}

export interface CloudSnapshot {
  mode?: string;
  mqtt_state: string;
  http_state: string;
  rtt_ms: number | null;
  drone_id: string;
  pairing_code: string;
}

export interface NetworkSnapshot {
  uplink?: string;
  rssi_dbm?: number | null;
  ip?: Record<string, string>;
}

export interface WfbRxSnapshot {
  adapter: string;
  channel: number;
  freq_mhz: number | null;
  rssi_dbm: number | null;
  packet_loss_pct: number | null;
  fec_recovered: number | null;
  fec_failed: number | null;
  bitrate_kbps: number | null;
  streams: unknown[];
}

export interface MeshSnapshot {
  role: string;
  batman_peers: unknown[];
  gateway_node: string | null;
  partition_state: string | null;
  mesh_addr: string | null;
}

export interface SourcesSnapshot {
  aggregated_kbps: number | null;
  frames_combined: number | null;
  frames_dedup: number | null;
  per_source: unknown[];
}

export interface ButtonsSnapshot {
  mapping: Record<string, string>;
  last_event: string | null;
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
}
