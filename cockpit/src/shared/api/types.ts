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
  /** An MSP FC's armed flag, read by the agent off a recent status reply to a
   *  poller on the MSP proxy; null when none was read lately (unknown). */
  fcMspArmed?: boolean | null;
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

// Loose shape — the agent returns a Pydantic dump with secrets redacted.
// We only consume the agent slice for log level and a couple of advanced
// fields. Everything else is opaque pass-through and ignored.
export interface AgentConfig {
  agent?: {
    profile?: string;
    // Injected by GET /api/config from the /etc/ados/board_override file the
    // HAL detector reads; empty string = auto-detect.
    board_override?: string;
  };
  logging?: {
    level?: string;
  };
  mavlink?: {
    source?: "auto" | "serial" | "udp" | "tcp";
    serial_port?: string;
    baud_rate?: number;
    system_id?: number;
    component_id?: number;
    ws_proxy_enforce_auth?: boolean;
    endpoints?: Array<{
      type?: string;
      host?: string;
      port?: number;
      enabled?: boolean;
    }>;
  };
  ground_station?: {
    role?: string;
  };
  network?: {
    wifi_client?: { ssid?: string };
    hotspot?: { enabled?: boolean };
    cellular?: { enabled?: boolean; apn?: string };
    mac_pin?: { enabled?: boolean; apply_live_allowed?: boolean };
    wifi_selfheal?: {
      enabled?: boolean;
      fail_threshold?: number;
      cooldown_s?: number;
    };
    regulatory?: RegulatoryInfo;
  };
  video?: {
    usb_recovery?: {
      enabled?: boolean;
      allow_ppps?: boolean;
      cold_boot_enum_aid?: boolean;
    };
    camera?: {
      // Image-orientation transforms applied before encode (see the Camera
      // settings page). rotation is clockwise degrees ∈ {0,90,180,270}.
      rotation?: number;
      hflip?: boolean;
      vflip?: boolean;
    };
  };
  server?: {
    mode?: string;
    self_hosted?: {
      url?: string;
      mqtt_broker?: string;
      mqtt_port?: number;
      api_key?: string;
    };
  };
  // GET /api/config redacts secrets: api_key comes back as the "***" sentinel
  // when set, "" when unset. The dashboard only reads the set/unset state.
  security?: {
    hmac_enabled?: boolean;
    setup_token_required?: boolean;
    api?: {
      api_key?: string;
      cors_enabled?: boolean;
    };
  };
  discovery?: {
    mdns_enabled?: boolean;
    service_type?: string;
  };
  vision?: {
    enabled?: boolean;
    backend?: string;
    confidence_threshold?: number;
    models_dir?: string;
    models_cache_max_mb?: number;
  };
  swarm?: {
    enabled?: boolean;
    role?: string;
    default_formation?: "line" | "column" | "wedge" | "grid" | "circle";
    default_spacing?: number;
  };
  // Battery health engine thresholds. Integers only: voltages in mV, the
  // temperature-rise rate in 0.1 °C/s. Read on the Battery settings page.
  battery?: {
    enabled?: boolean;
    low_cell_mv?: number;
    critical_cell_mv?: number;
    cell_divergence_mv?: number;
    voltage_drop_mv_per_s?: number;
    temp_spike_dc_per_s?: number;
    predictive_window_s?: number;
    reserve_percent?: number;
  };
  [key: string]: unknown;
}
