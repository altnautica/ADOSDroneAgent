// `GET /api/v1/peripherals` as the Peripherals screen shows it: one row per
// registered peripheral with its connection state, a tone for the state dot,
// and the detail line. Pure, so the state rules are unit-testable.

import type { Tone } from "@/lib/tone";

export interface Peripheral {
  id: string;
  name: string;
  connected: boolean;
  /** The registry's own state word (API data, shown as-is), or null. */
  state: string | null;
  tone: Tone;
  detail: string[];
  description: string | null;
  /** Unix seconds of the last sighting, when disconnected. */
  lastSeen: number | null;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(v: unknown): string | null {
  return typeof v === "string" && v ? v : null;
}

const CONNECTED_STATES: Record<string, true> = { connected: true, ready: true, active: true };

/** Parse the list. A top-level `connected` boolean wins; older agents report
 *  only a `state`/`status` word. */
export function parsePeripherals(body: unknown): Peripheral[] {
  const rows = isObj(body) && Array.isArray(body.peripherals) ? body.peripherals : [];
  return rows.flatMap((r: unknown): Peripheral[] => {
    if (!isObj(r) || !str(r.id)) return [];
    const manifest = isObj(r.manifest) ? r.manifest : {};
    const extra = isObj(r.extra) ? r.extra : {};
    const state = str(r.state) ?? str(r.status);
    const connected = typeof r.connected === "boolean" ? r.connected : state !== null && CONNECTED_STATES[state] === true;
    let tone: Tone = "muted";
    if (connected) tone = "ok";
    else if (state === "error" || state === "failed") tone = "err";
    else if (state === "missing" || state === "disconnected" || r.connected === false) tone = "warn";
    return [
      {
        id: r.id as string,
        name: str(r.display_name) ?? str(r.name) ?? (r.id as string),
        connected,
        state,
        tone,
        detail: [str(r.transport), str(manifest.kind), str(manifest.vendor), str(extra.category)].filter(
          (s): s is string => s !== null,
        ),
        description: str(manifest.description) ?? str(extra.description),
        lastSeen: !connected && typeof r.last_seen === "number" ? r.last_seen : null,
      },
    ];
  });
}

/** Elapsed time since `unix` as a count and unit for the locale string. */
export function sinceParts(unix: number, nowMs = Date.now()): { n: number; unit: "seconds" | "minutes" | "hours" | "days" } {
  const delta = Math.max(0, nowMs / 1000 - unix);
  if (delta < 60) return { n: Math.floor(delta), unit: "seconds" };
  if (delta < 3600) return { n: Math.floor(delta / 60), unit: "minutes" };
  if (delta < 86400) return { n: Math.floor(delta / 3600), unit: "hours" };
  return { n: Math.floor(delta / 86400), unit: "days" };
}
