// The MAVLink settings page model. Every key here can drop the flight
// controller link, so each change is held behind a guarded button. The agent
// owns the armed interlock: it refuses a link key while armed (409 E_ARMED),
// and the page asks the operator before repeating the write with `force`. The
// page only shows the arm state (from `GET /api/telemetry`) as a banner.

import type { VehicleState } from "@/lib/types";
import { putConfigChecked, type ConfigWriteResult } from "@/shared/api/apply-actions";
import { withArmedRetry } from "@/shared/api/armed-retry";

export const MAVLINK_SOURCES = ["auto", "serial", "udp", "tcp"] as const;
export type MavlinkSource = (typeof MAVLINK_SOURCES)[number];

export const SOURCE_LABEL = {
  auto: { labelKey: "settings.mavlink.sourceAutoLabel", descriptionKey: "settings.mavlink.sourceAutoDescription" },
  serial: { labelKey: "settings.mavlink.sourceSerialLabel", descriptionKey: "settings.mavlink.sourceSerialDescription" },
  udp: { labelKey: "settings.mavlink.sourceUdpLabel", descriptionKey: "settings.mavlink.sourceUdpDescription" },
  tcp: { labelKey: "settings.mavlink.sourceTcpLabel", descriptionKey: "settings.mavlink.sourceTcpDescription" },
} as const satisfies Record<MavlinkSource, { labelKey: string; descriptionKey: string }>;

export const MAVLINK_KEYS = {
  source: "mavlink.source",
  serialPort: "mavlink.serial_port",
  baudRate: "mavlink.baud_rate",
  systemId: "mavlink.system_id",
  componentId: "mavlink.component_id",
} as const;

/** How often the page re-reads the armed state. */
export const ARMED_POLL_MS = 1000;

// Example endpoint values, not copy.
export const NETWORK_TARGET_EXAMPLE = "udp:127.0.0.1:14550";
export const SERIAL_PORT_EXAMPLE = "/dev/ttyACM0";

export const ID_BOUNDS = { min: 1, max: 255 } as const;
export const MIN_BAUD = 1200;

/** The standard flight-controller serial rates offered in the picker. */
export const BAUD_RATES = [9600, 19200, 38400, 57600, 115200, 230400, 460800, 500000, 921600, 1500000] as const;

export function toSource(v: unknown): MavlinkSource {
  return (MAVLINK_SOURCES as readonly unknown[]).includes(v) ? (v as MavlinkSource) : "auto";
}

/** Which transport fields apply for a source: auto probes on its own, a
 *  network source takes its target in the port field and has no baud. */
export function transportFields(source: MavlinkSource): { isNetwork: boolean; targetEnabled: boolean; baudEnabled: boolean } {
  const isNetwork = source === "udp" || source === "tcp";
  return { isNetwork, targetEnabled: source !== "auto", baudEnabled: source === "serial" };
}

/** The baud picker's options: the standard rates plus the stored rate when it
 *  is a non-standard one, ascending. */
export function baudOptions(current: number | null | undefined): string[] {
  const set = new Set<number>(BAUD_RATES);
  if (current != null && Number.isInteger(current) && current >= MIN_BAUD) set.add(current);
  return [...set].sort((a, b) => a - b).map(String);
}

/** The vehicle's arm state from the live read: true or false when the agent
 *  reports it, null while it is unknown (no read yet, or the read failed). */
export function armedFrom(telemetry: VehicleState | null, stale: boolean): boolean | null {
  if (telemetry == null || stale) return null;
  return telemetry.armed === true;
}

export type MavlinkBanner = "none" | "armed" | "unknown";

/** The arm-state banner the page shows. It never blocks a change: the agent
 *  decides, and an armed refusal goes to the operator. */
export function mavlinkBanner(armed: boolean | null): MavlinkBanner {
  if (armed === null) return "unknown";
  return armed ? "armed" : "none";
}

/** Write one MAVLink leaf; on the agent's armed refusal ask `confirm` and,
 *  only on a yes, repeat the write with `force`. */
export function writeMavlinkLeaf(
  key: string,
  value: string,
  confirm: (msg: string) => Promise<boolean>,
  put: typeof putConfigChecked = putConfigChecked,
): Promise<ConfigWriteResult> {
  return withArmedRetry((force) => put(key, value, force ? { force } : {}), confirm);
}

export interface MavlinkEndpoint {
  type?: string;
  host?: string;
  port?: number;
  enabled?: boolean;
}

/** One advertised endpoint as `host:port`, with the disabled suffix. */
export function endpointValue(ep: MavlinkEndpoint, disabledSuffix: string): string {
  return `${ep.host ?? "0.0.0.0"}:${ep.port ?? "?"}${ep.enabled === false ? disabledSuffix : ""}`;
}
