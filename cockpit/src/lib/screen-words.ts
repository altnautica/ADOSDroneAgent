// Wire enums the status screens show, as locale keys: sensor kinds and
// sources, the sensor-hub link, peripheral states, compute-node job and
// offload-session states, the LoRa region source, radio transport and rules. A word
// outside the agent's closed set returns null, and the caller shows it as
// sent rather than guess a label.

const SENSOR_KINDS = ["imu", "camera", "thermal", "lidar", "gnss", "baro", "mag"] as const;
const SENSOR_SOURCES = ["hub", "fc", "vision", "lidar", "gnss", "sim", "host"] as const;
const JOB_STATES = ["queued", "running", "done", "failed", "cancelled", "interrupted"] as const;
const SESSION_STATES = ["opening", "live", "stalled"] as const;
const LORA_TRANSPORTS = ["e22", "meshtastic", "loopback"] as const;
const LORA_RULES = ["duty_cycle", "dwell_time", "lbt", "max_power"] as const;
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

function pick<T extends string>(v: string | null | undefined, set: readonly T[]): T | null {
  return v != null && (set as readonly string[]).includes(v) ? (v as T) : null;
}

// Each key is spelled out as a template so the locale key check sees its
// prefix rather than a bare namespace.
export function sensorKindKey(v: string | null | undefined) {
  const w = pick(v, SENSOR_KINDS);
  return w && (`cockpit.sensors.kind.${w}` as const);
}
export function sensorSourceKey(v: string | null | undefined) {
  const w = pick(v, SENSOR_SOURCES);
  return w && (`cockpit.sensors.source.${w}` as const);
}
export function jobStateKey(v: string | null | undefined) {
  const w = pick(v, JOB_STATES);
  return w && (`cockpit.node.jobState.${w}` as const);
}
export function sessionStateKey(v: string | null | undefined) {
  const w = pick(v, SESSION_STATES);
  return w && (`cockpit.node.sessionState.${w}` as const);
}
export function loraRuleKey(v: string | null | undefined) {
  const w = pick(v, LORA_RULES);
  return w && (`cockpit.lora.ruleName.${w}` as const);
}
export function loraTransportKey(v: string | null | undefined) {
  const w = pick(v, LORA_TRANSPORTS);
  return w && (`cockpit.lora.transport.${w}` as const);
}

export function loraSourceKey(v: string | null | undefined) {
  const w = v != null && v in LORA_SOURCES ? LORA_SOURCES[v as keyof typeof LORA_SOURCES] : null;
  return w && (`cockpit.lora.source.${w}` as const);
}

/** A peripheral's state label: connected wins, a missing word reads unknown,
 *  and a word outside the registry's set returns null (shown as sent). */
export function peripheralStateKey(p: { connected: boolean; state: string | null }) {
  const k = p.connected ? "connected" : (p.state ?? "unknown").toLowerCase();
  const w = k in PERIPHERAL_STATES ? PERIPHERAL_STATES[k as keyof typeof PERIPHERAL_STATES] : null;
  return w && (`cockpit.peripherals.state.${w}` as const);
}

/** The sensor-hub link (`serial:/dev/ttyACM0`, `tcp:host:port`) as a key and
 *  its target, or null for a word the hub service does not produce. */
export function hubTransportWord(v: string | null | undefined) {
  const m = /^(serial|tcp):(.+)$/.exec(v ?? "");
  if (!m) return null;
  const key = m[1] === "serial" ? ("cockpit.sensors.hubSerial" as const) : ("cockpit.sensors.hubSimulated" as const);
  return { key, target: m[2] };
}
