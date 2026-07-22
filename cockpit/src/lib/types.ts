// Shared cockpit types. The status shapes mirror the agent's native
// `GET /api/v1/ground-station/status` composite (crates/ados-control) so the
// panel can never disagree with the on-board TFT/OLED. Fields the agent may
// send as null are typed nullable; unknown extras are tolerated.

/** The WFB radio link view (the `link` block of the GS status composite). */
export interface LinkView {
  rssi_dbm: number | null;
  bitrate_mbps?: number | null;
  bitrate_kbps: number | null;
  fec_recovered: number;
  fec_lost?: number;
  fec_failed: number;
  channel: number | null;
  snr_db: number | null;
  noise_dbm: number | null;
  packets_received: number;
  packets_lost: number;
  loss_percent: number | null;
  tx_power_dbm: number | null;
  /** e.g. "connected" | "connecting" | "degraded" | "rf_unverified". */
  state: string;
  /** The one-glance link diagnosis the agent derives from its decode counters
   *  ("healthy" | "searching" | "deaf" | "mis_keyed" | "jammed"). Null until the
   *  agent classifies the link; absent on older agents. */
  link_diag?: string | null;
  /** Every frame captured off-air BEFORE decrypt (0 = a deaf radio, no RF). */
  packets_all?: number | null;
  /** Decrypt / session failures (wrong key or wrong link_id / channel_id). */
  decrypt_errors?: number | null;
}

/** The distributed-RX role block. */
export interface RoleBlock {
  current: string;
  configured: string;
  supported: string[];
  mesh_capable: boolean;
}

/** The uplink / AP network view. */
export interface NetworkView {
  ap_ssid: string | null;
  ap_ip: string | null;
  usb_ip: string | null;
  uplink_type: string | null;
  uplink_reachable: boolean;
}

/** Box health snapshot. */
export interface SystemView {
  cpu_pct: number | null;
  ram_used_mb: number | null;
  ram_total_mb: number | null;
  temp_c: number | null;
  uptime_seconds: number | null;
  agent_version: string | null;
}

/** The identity of the paired drone, when one is paired. */
export interface PairedDrone {
  device_id: string | null;
  key_fingerprint: string | null;
  fc_mode: string | null;
  battery_pct: number | null;
  gps_sats: number | null;
}

/** The self-healing mesh block. */
export interface MeshView {
  up: boolean;
  peer_count: number;
  selected_gateway: string | null;
  partition: boolean;
  mesh_id: string | null;
}

/** The full composite from `GET /api/v1/ground-station/status`. */
export interface GsStatus {
  profile: string;
  paired_drone: PairedDrone;
  link: LinkView;
  gcs: { clients: unknown[]; pic_id: string | null };
  network: NetworkView;
  system: SystemView;
  recording: boolean;
  video: { recording: boolean; recording_filename: string | null };
  role: RoleBlock;
  mesh: MeshView;
}

/** A physical front-panel button event, forwarded verbatim from the agent's
 *  `ados-pic` button fanout over `/ws/buttons`. `kind` is the short/long/cancel
 *  classification; `action` is the press/hold/release phase. The cockpit owns
 *  the mapping from these to menu navigation. */
export interface ButtonEvent {
  button: string;
  kind?: string;
  action?: string;
  timestamp_ms?: number;
}

/** The paired drone's live vehicle state — the shape of `GET /api/telemetry`
 *  (the MAVLink router's `to_wire` snapshot the laptop dashboard also consumes).
 *  On a ground station this is the drone's state received over the radio link
 *  and republished locally. Every leaf is nullable; the endpoint returns `{}`
 *  when no vehicle has been heard, so all blocks are optional. Attitude angles
 *  are RADIANS (raw MAVLink ATTITUDE); position/velocity are metres and m/s;
 *  heading is degrees; `battery.remaining` is a percentage with `-1` meaning
 *  unknown. */
export interface VehicleAttitude {
  roll: number | null;
  pitch: number | null;
  yaw: number | null;
}

export interface VehiclePosition {
  lat: number | null;
  lon: number | null;
  alt_msl: number | null;
  alt_rel: number | null;
  heading: number | null;
}

export interface VehicleVelocity {
  vx: number | null;
  vy: number | null;
  vz: number | null;
  groundspeed: number | null;
  airspeed: number | null;
  climb: number | null;
}

export interface VehicleBattery {
  voltage: number | null;
  current: number | null;
  remaining: number | null;
  temperature: number | null;
}

export interface VehicleGps {
  fix_type: number | null;
  satellites: number | null;
  eph: number | null;
  epv: number | null;
}

export interface VehicleState {
  mav_type?: number | null;
  autopilot?: number | null;
  armed?: boolean;
  mode?: string | null;
  position?: VehiclePosition;
  velocity?: VehicleVelocity;
  attitude?: VehicleAttitude;
  battery?: VehicleBattery;
  gps?: VehicleGps;
  /** Distance to the launch/home point in metres, when the agent supplies it.
   *  Absent today (the vehicle snapshot carries no home), so the HUD renders a
   *  dash rather than fabricating a distance. */
  home_distance?: number | null;
  /** ISO 8601 stamp of the last message / heartbeat received. On-box the panel
   *  shares the agent's clock, so these gate attitude freshness reliably. */
  last_update?: string;
  last_heartbeat?: string;
}

/** One entry of the `GET /api/video/roster` reconciled camera list. Only the
 *  fields the Feed's multi-stream tabs need; unknown extras are tolerated. A
 *  ground station serves an empty roster (it has no onboard camera), so the
 *  tabs never appear there. */
export interface RosterCamera {
  id: string;
  label?: string;
  name?: string;
  role?: string;
  live?: boolean;
  /** Per-leg WHEP endpoint, when the agent advertises one. The primary leg is
   *  reached through the fixed `/whep` proxy. */
  whep_url?: string;
}

/** A minimal, tolerant view of `GET /api/video/config` — only the blocks the
 *  status strip's video zone reads. `encoder` is the box's OWN configured
 *  encoder (honest as "the feed" only on a drone, the video source); `link`
 *  carries the receiver's measured inbound video byte rate (honest on a ground
 *  station). Unknown extra keys are tolerated. */
export interface VideoConfigResponse {
  encoder?: {
    bitrate_kbps?: number | null;
    width?: number | null;
    height?: number | null;
    fps?: number | null;
    codec?: string | null;
  };
  link?: {
    video_inbound_bytes_per_s?: number | null;
    channel?: number | null;
  };
}

/** The agent config is a nested JSON object (`GET /api/config`, sanitized
 *  Pydantic model_dump). The Settings tree renders it in a later stage. */
export type ConfigValue =
  | string
  | number
  | boolean
  | null
  | ConfigValue[]
  | { [key: string]: ConfigValue };

export type AgentConfig = { [key: string]: ConfigValue };
