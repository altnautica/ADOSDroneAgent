// The Settings tree's static knowledge, kept out of the components so the tree
// itself is pure rendering. There is NO agent-config metadata registry (the
// flight-controller parameter-metadata registry is for FC params, not agent
// config), so the tree infers each leaf's widget from the JSON value type on the
// config dump plus the small hint tables here:
//
//   - ENUM_OPTIONS   the exact legal values for the config fields that are a
//                    Pydantic `Literal`/`Enum` (a JSON dump erases those to a
//                    plain string, so the allowed set is listed here);
//   - NUMBER_BOUNDS  the min/max/int hints for the fields the config models
//                    bound (Pydantic `Field(ge=, le=)`), so the on-screen numpad
//                    can clamp and refuse a decimal on an integer field;
//   - REBOOT_*       the paths a service reads only at startup, so a change is
//                    honestly flagged as reboot-pending rather than shown live;
//   - REDACTED_PATHS the secret paths the config GET returns as `***` — the
//                    editor starts them empty and never writes the sentinel back;
//   - CURATED_GROUPS the field grouping that leads the tree (the common set),
//                    with an "All settings" node exposing the whole raw dump.
//
// Anything not covered by a hint falls back to the JSON-type-inferred widget, so
// every field stays reachable and editable — nothing is unreachable.

import {
  Cloud,
  Camera,
  Cpu,
  MonitorSmartphone,
  Network,
  Radio,
  Share2,
  SlidersHorizontal,
  UserCog,
  type LucideIcon,
} from "lucide-react";

import type { AgentConfig, ConfigValue } from "@/lib/types";

// ── path utilities ──────────────────────────────────────────────────────────

/** Read the value at a dot-path in the config dump, or `undefined` when any
 *  segment is missing (a curated leaf absent on this profile is skipped). */
export function getAtPath(
  config: AgentConfig | null,
  dotpath: string,
): ConfigValue | undefined {
  if (!config || !dotpath) return config ?? undefined;
  let node: ConfigValue = config;
  for (const seg of dotpath.split(".")) {
    if (node == null || typeof node !== "object" || Array.isArray(node)) {
      return undefined;
    }
    if (!(seg in node)) return undefined;
    node = (node as Record<string, ConfigValue>)[seg];
  }
  return node;
}

/** True for a plain object (a group that drills in), false for arrays/scalars. */
export function isGroup(v: ConfigValue | undefined): v is Record<string, ConfigValue> {
  return v != null && typeof v === "object" && !Array.isArray(v);
}

/** The kind of widget a leaf gets, inferred from its JSON value + the hints. */
export type LeafKind = "toggle" | "enum" | "number" | "text" | "list";

/** Resolve the widget kind for a leaf value at a path. */
export function leafKind(dotpath: string, value: ConfigValue | undefined): LeafKind {
  if (Array.isArray(value)) return "list";
  if (typeof value === "boolean") return "toggle";
  if (dotpath in ENUM_OPTIONS) return "enum";
  if (typeof value === "number") return "number";
  return "text"; // string | null (optional) | anything else
}

// ── acronyms + label prettifier ─────────────────────────────────────────────

const ACRONYMS: Record<string, string> = {
  wfb: "WFB", rssi: "RSSI", snr: "SNR", ip: "IP", ssid: "SSID", mcs: "MCS",
  fec: "FEC", api: "API", url: "URL", id: "ID", cpu: "CPU", ram: "RAM",
  ui: "UI", tls: "TLS", cors: "CORS", apn: "APN", mdns: "mDNS", dbm: "dBm",
  ghz: "GHz", mhz: "MHz", iso: "ISO", gcs: "GCS", npu: "NPU", fps: "FPS",
  kbps: "kbps", hmac: "HMAC", mqtt: "MQTT", usb: "USB", os: "OS", lcd: "LCD",
  hdmi: "HDMI", oled: "OLED", dns: "DNS", dhcp: "DHCP", nat: "NAT", sei: "SEI",
  vbus: "VBUS", gs: "GS", fc: "FC", rtp: "RTP", rest: "REST", ados: "ADOS",
  wifi: "WiFi", mavlink: "MAVLink", ttl: "TTL", ipc: "IPC", ttyl: "TTL",
  bat: "bat", psk: "PSK",
};

/** A config key segment → a human label ("wfb" → "WFB", "reg_domain" →
 *  "Reg Domain", "ground_station" → "Ground Station"). */
export function prettify(segment: string): string {
  return segment
    .split(/[_-]/)
    .filter(Boolean)
    .map((w) => ACRONYMS[w.toLowerCase()] ?? w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

/** A dot-path → a readable trail ("network.regulatory" → "Network › Regulatory"). */
export function prettyTrail(dotpath: string): string {
  return dotpath.split(".").map(prettify).join(" › ");
}

// ── enum options (Pydantic Literal / Enum fields, plus a few known str sets) ──

export const ENUM_OPTIONS: Record<string, string[]> = {
  "agent.profile": ["auto", "drone", "ground_station", "workstation", "compute"],
  "mavlink.source": ["auto", "serial", "udp", "tcp"],
  "video.camera.codec_preference": ["h264", "h265", "auto"],
  "video.camera.expected": ["auto", "true", "false"],
  "video.wfb.topology": ["host_vbus", "powered_hub", "external_5v"],
  "video.wfb.band": ["u-nii-1", "u-nii-3", "all"],
  "video.wfb.wfb_link_preset": ["conservative", "balanced", "aggressive"],
  "network.regulatory.mode": ["unrestricted", "region"],
  "server.mode": ["cloud", "self_hosted", "local"],
  "server.mqtt_transport": ["tcp", "websockets"],
  "remote_access.provider": ["none", "cloudflare"],
  "atlas.capture_profile": ["orbit", "lawnmower", "freeform", "inspection"],
  "atlas.pose_tier": ["auto", "local", "offload", "hybrid"],
  "perception.offload.enabled": ["auto", "on", "off"],
  "perception.serving.enabled": ["auto", "on", "off"],
  "ground_station.role": ["direct", "relay", "receiver"],
  "ground_station.cloud_uplink": ["auto", "force_on", "force_off"],
  "ground_station.mesh.carrier": ["802.11s", "ibss"],
  "ground_station.display.type": ["auto", "hdmi", "lcd", "none"],
  "ui.theme": ["dark", "light"],
  "logging.level": ["debug", "info", "warning", "error"],
};

// ── number bounds (Pydantic Field(ge=, le=) + sane operating ranges) ─────────

export interface NumberBound {
  min?: number;
  max?: number;
  step?: number;
  /** Integer-only field: the numpad hides the decimal point and rounds. */
  int?: boolean;
}

export const NUMBER_BOUNDS: Record<string, NumberBound> = {
  "video.lcd_fps_cap": { min: 1, max: 60, int: true },
  "vision.confidence_threshold": { min: 0, max: 1, step: 0.05 },
  "video.wfb.tx_power_dbm": { min: 1, max: 30, int: true },
  "video.wfb.tx_power_max_dbm": { min: 1, max: 30, int: true },
  "video.wfb.mcs_index": { min: 0, max: 11, int: true },
  "video.wfb.channel": { min: 1, max: 196, int: true },
  "video.wfb.fec_k": { min: 1, max: 32, int: true },
  "video.wfb.fec_n": { min: 1, max: 32, int: true },
  "video.wfb.hop_period_seconds": { min: 5, max: 3600, int: true },
  "video.wfb.hop_loss_threshold_percent": { min: 0, max: 100 },
  "video.wfb.hop_rssi_threshold_dbm": { min: -100, max: 0 },
  "video.camera.width": { min: 160, max: 3840, int: true },
  "video.camera.height": { min: 120, max: 2160, int: true },
  "video.camera.fps": { min: 1, max: 120, int: true },
  "video.camera.bitrate_kbps": { min: 250, max: 50000, int: true },
  "video.cloud_rtp_port": { min: 1, max: 65535, int: true },
  "api.rest.port": { min: 1, max: 65535, int: true },
  "mavlink.baud_rate": { min: 1200, max: 2000000, int: true },
  "mavlink.system_id": { min: 1, max: 255, int: true },
  "mavlink.component_id": { min: 1, max: 255, int: true },
  "ground_station.mesh.channel": { min: 1, max: 196, int: true },
  "ground_station.wfb_relay.receiver_port": { min: 1, max: 65535, int: true },
  "ground_station.wfb_receiver.listen_port": { min: 1, max: 65535, int: true },
  "network.hotspot.channel": { min: 1, max: 196, int: true },
  "logging.max_size_mb": { min: 1, max: 4096, int: true },
  "logging.keep_count": { min: 1, max: 100, int: true },
  "pairing.beacon_interval": { min: 5, max: 3600, int: true },
  "pairing.heartbeat_interval": { min: 5, max: 3600, int: true },
  "server.telemetry_rate": { min: 1, max: 50, int: true },
  "server.heartbeat_interval": { min: 1, max: 3600, int: true },
};

// ── reboot-required paths (a service reads them only at startup) ─────────────
//
// Flagged generously and in the safe direction: a service that reads a value
// only at startup must be restarted for a change to take effect, so the tree
// warns "reboot to apply" rather than let the operator believe a stored value
// is already live. Over-warning is honest; under-warning is the false surface
// to avoid. Booleans still flip inline — the flip persists the value and, when
// the path is here, raises the pending-reboot banner.

const REBOOT_PREFIXES: readonly string[] = [
  "video.", // the whole video/WFB pipeline is read when ados-video starts
  "network.", // the network managers read their config at startup
  "mavlink.", // FC transport — read when the router starts
  "server.", // cloud posture — services gate at startup
  "api.rest.", // the HTTP listener binds these at startup
  "ground_station.mesh.",
  "ground_station.wfb_relay.",
  "ground_station.wfb_receiver.",
  "ground_station.kiosk.",
  "ground_station.ui.",
  "security.tls.",
  "security.wireguard.",
  "remote_access.",
  "vision.",
  "atlas.",
  "swarm.",
  "pairing.",
  "discovery.",
  "logging.",
];

const REBOOT_EXACT: ReadonlySet<string> = new Set([
  "agent.profile",
  "agent.tier",
  "agent.board_override",
  "ground_station.role",
  "ground_station.cloud_uplink",
  "ground_station.display.type",
  "ground_station.share_uplink",
]);

/** True when a change to `dotpath` only takes effect after a service restart or
 *  reboot — so the tree flags it (never shows a reboot-gated change as live). */
export function needsReboot(dotpath: string): boolean {
  if (REBOOT_EXACT.has(dotpath)) return true;
  return REBOOT_PREFIXES.some((p) => dotpath.startsWith(p));
}

// ── read-only paths (surfaced by the config GET but not writable via PUT) ─────
//
// `agent.board_override` is injected into the GET response from the
// /etc/ados/board_override file — it is NOT a field on the Pydantic config, so a
// `PUT /api/config agent.board_override` returns "Key not found". The tree shows
// it but never offers an edit that is guaranteed to fail.

export const READ_ONLY_PATHS: ReadonlySet<string> = new Set(["agent.board_override"]);

export function isReadOnly(dotpath: string): boolean {
  return READ_ONLY_PATHS.has(dotpath);
}

// ── redacted secret paths (GET returns `***`; never write the sentinel) ──────

export const REDACTION_SENTINEL = "***";

export const REDACTED_PATHS: ReadonlySet<string> = new Set([
  "security.tls.key_path",
  "security.api.api_key",
  "security.wireguard.config_path",
  "server.self_hosted.api_key",
]);

export function isRedacted(dotpath: string): boolean {
  return REDACTED_PATHS.has(dotpath);
}

// ── curated groups (the common set leads; "All settings" holds the long tail) ─

export interface CuratedGroup {
  /** Group id, addressed as `~<id>`. */
  id: string;
  label: string;
  icon: LucideIcon;
  description: string;
  /** Ordered dot-paths — each a scalar leaf (editor) or a nested object (drill).
   *  A path absent on the current profile's config is skipped at render time. */
  paths: string[];
}

export const CURATED_GROUPS: CuratedGroup[] = [
  {
    id: "profile",
    label: "Profile & Role",
    icon: UserCog,
    description: "Identity, profile, distributed-RX role",
    paths: [
      "agent.name",
      "agent.profile",
      "agent.tier",
      "agent.board_override",
      "ground_station.role",
      "ground_station.cloud_uplink",
    ],
  },
  {
    id: "network",
    label: "Network & Uplink",
    icon: Network,
    description: "Regulatory posture, WiFi, cellular, sharing",
    paths: [
      "network.regulatory.mode",
      "network.regulatory.region",
      "network.wifi_client",
      "network.hotspot",
      "network.cellular",
      "ground_station.share_uplink",
      "network.mac_pin",
      "network.wifi_selfheal",
    ],
  },
  {
    id: "wfb",
    label: "Radio (WFB)",
    icon: Radio,
    description: "Channel, region, power, FEC, hopping",
    paths: [
      "video.wfb.channel",
      "video.wfb.band",
      "video.wfb.reg_domain",
      "video.wfb.tx_power_dbm",
      "video.wfb.tx_power_max_dbm",
      "video.wfb.mcs_index",
      "video.wfb.wfb_link_preset",
      "video.wfb.topology",
      "video.wfb.auto_channel_enabled",
      "video.wfb.auto_hop_enabled",
      "video.wfb.adaptive_bitrate_enabled",
      "video.wfb.auto_pair_enabled",
      "video.wfb.sei_latency",
    ],
  },
  {
    id: "mesh",
    label: "Mesh",
    icon: Share2,
    description: "batman-adv carrier, relay + receiver roles",
    paths: [
      "ground_station.mesh.carrier",
      "ground_station.mesh.channel",
      "ground_station.mesh.mesh_id",
      "ground_station.mesh.bat_iface",
      "ground_station.wfb_relay",
      "ground_station.wfb_receiver",
    ],
  },
  {
    id: "display",
    label: "Display & Kiosk",
    icon: MonitorSmartphone,
    description: "Display path, HDMI kiosk, theme, buttons",
    paths: [
      "ground_station.display.type",
      "ground_station.kiosk.enabled",
      "ground_station.kiosk.target_url",
      "ground_station.kiosk.resolution",
      "ground_station.kiosk.minimal_layer",
      "ui.theme",
      "ground_station.ui",
    ],
  },
  {
    id: "cloud",
    label: "Cloud",
    icon: Cloud,
    description: "Cloud posture, self-hosted, pairing beacon",
    paths: [
      "server.mode",
      "server.cloud",
      "server.self_hosted",
      "pairing.beacon_enabled",
      "pairing.convex_url",
      "remote_access.provider",
      "remote_access.cloudflare",
    ],
  },
  {
    id: "perception",
    label: "Perception & Offload",
    icon: Cpu,
    description: "Vision, Atlas, offload + serving",
    paths: [
      "perception.offload.enabled",
      "perception.offload.compute_node_addr",
      "perception.serving.enabled",
      "perception.serving.detector_model",
      "vision.enabled",
      "vision.backend",
      "vision.confidence_threshold",
      "atlas.enabled",
    ],
  },
  {
    id: "camera",
    label: "Camera & Recording",
    icon: Camera,
    description: "Encode format, resolution, recording",
    paths: [
      "video.camera.source",
      "video.camera.codec",
      "video.camera.width",
      "video.camera.height",
      "video.camera.fps",
      "video.camera.bitrate_kbps",
      "video.camera.codec_preference",
      "video.recording.enabled",
      "video.recording.max_duration_minutes",
      "video.lcd_fps_cap",
      "video.prefer_hw_encoder",
      "video.use_gst_air_pipeline",
    ],
  },
  {
    id: "system",
    label: "System & Advanced",
    icon: SlidersHorizontal,
    description: "Logging, MAVLink, API, discovery",
    paths: [
      "logging.level",
      "logging.max_size_mb",
      "api.rest.port",
      "api.mission_control_url",
      "mavlink.source",
      "mavlink.baud_rate",
      "mavlink.system_id",
      "mavlink.ws_proxy_enforce_auth",
      "discovery.mdns_enabled",
      "security.tls.enabled",
      "pairing.beacon_interval",
    ],
  },
];

export function curatedGroupById(id: string): CuratedGroup | undefined {
  return CURATED_GROUPS.find((g) => g.id === id);
}
