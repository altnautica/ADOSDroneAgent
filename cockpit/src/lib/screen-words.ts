// Wire enums the status screens show, as locale keys: sensor kinds and
// sources, the sensor-hub link, peripheral states, compute-node job and
// offload-session states, the LoRa region source, radio transport and rules,
// systemd service states and the capture session state. A word outside the
// agent's closed set reads "Unrecognised", never as sent; a missing word
// returns null and the caller shows a dash.

const SENSOR_KINDS = ["imu", "camera", "thermal", "lidar", "gnss", "baro", "mag"] as const;
const SENSOR_SOURCES = ["hub", "fc", "vision", "lidar", "gnss", "sim", "host"] as const;
const JOB_STATES = ["queued", "running", "done", "failed", "cancelled", "interrupted"] as const;
const SESSION_STATES = ["opening", "live", "stalled"] as const;
const LORA_TRANSPORTS = ["e22", "meshtastic", "loopback"] as const;
const LORA_RULES = ["duty_cycle", "dwell_time", "lbt", "max_power"] as const;
const CAPTURE_STATES = ["idle", "recording", "paused", "closing"] as const;
const SERVICE_STATES = ["active", "inactive", "failed", "activating", "deactivating", "reloading"] as const;
const UNRECOGNISED = "panels.state.unrecognised" as const;
const LORA_SOURCES = { "lora.region": "lora", "network.regulatory.region": "country" } as const;
/** Peripheral registry words (any case) → the label the screen shows. */
const PERIPHERAL_STATES = {
  connected: "connected",
  ready: "connected",
  active: "connected",
  disconnected: "disconnected",
  missing: "notDetected",
  not_detected: "notDetected",
  error: "error",
  failed: "error",
  unknown: "unknown",
} as const;

/** The word when it is in `set`, `unrecognised` when it is not, null when
 *  the wire sent none. */
function pick<T extends string>(v: string | null | undefined, set: readonly T[]): T | "unrecognised" | null {
  if (v == null || v === "") return null;
  return (set as readonly string[]).includes(v) ? (v as T) : "unrecognised";
}

// Each key is spelled out as a template so the locale key check sees its
// prefix rather than a bare namespace.
export function sensorKindKey(v: string | null | undefined) {
  const w = pick(v, SENSOR_KINDS);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.sensors.kind.${w}` as const);
}
export function sensorSourceKey(v: string | null | undefined) {
  const w = pick(v, SENSOR_SOURCES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.sensors.source.${w}` as const);
}
export function jobStateKey(v: string | null | undefined) {
  const w = pick(v, JOB_STATES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.node.jobState.${w}` as const);
}
export function sessionStateKey(v: string | null | undefined) {
  const w = pick(v, SESSION_STATES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.node.sessionState.${w}` as const);
}
export function loraRuleKey(v: string | null | undefined) {
  const w = pick(v, LORA_RULES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.lora.ruleName.${w}` as const);
}
export function loraTransportKey(v: string | null | undefined) {
  const w = pick(v, LORA_TRANSPORTS);
  return w === "unrecognised" ? UNRECOGNISED : w && (`cockpit.lora.transport.${w}` as const);
}

export function loraSourceKey(v: string | null | undefined) {
  if (v == null || v === "") return null;
  if (!(v in LORA_SOURCES)) return UNRECOGNISED;
  return `cockpit.lora.source.${LORA_SOURCES[v as keyof typeof LORA_SOURCES]}` as const;
}

/** A systemd unit's active state, in any case. */
export function serviceStateKey(v: string | null | undefined) {
  const w = pick(v?.toLowerCase(), SERVICE_STATES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`panels.state.service.${w}` as const);
}

/** The capture session's state. */
export function captureStateKey(v: string | null | undefined) {
  const w = pick(v, CAPTURE_STATES);
  return w === "unrecognised" ? UNRECOGNISED : w && (`capture.state.${w}` as const);
}

/** A peripheral's state label: connected wins, a missing word reads unknown,
 *  and a word outside the registry's set reads unrecognised. */
export function peripheralStateKey(p: { connected: boolean; state: string | null }) {
  const k = p.connected ? "connected" : (p.state ?? "unknown").toLowerCase();
  if (!(k in PERIPHERAL_STATES)) return UNRECOGNISED;
  return `cockpit.peripherals.state.${PERIPHERAL_STATES[k as keyof typeof PERIPHERAL_STATES]}` as const;
}

/** The sensor-hub link (`serial:/dev/ttyACM0`, `tcp:host:port`) as a key and
 *  its target; a link the hub service does not produce reads unrecognised, and
 *  a missing one is null. */
export function hubTransportWord(v: string | null | undefined) {
  if (v == null || v === "") return null;
  const m = /^(serial|tcp):(.+)$/.exec(v);
  if (!m) return { key: UNRECOGNISED, target: "" };
  const key = m[1] === "serial" ? ("cockpit.sensors.hubSerial" as const) : ("cockpit.sensors.hubSimulated" as const);
  return { key, target: m[2] };
}
