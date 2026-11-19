// The ground station's radio uplink: whether this box transmits to the fleet,
// or a phone took the uplink and the box only receives. The radio service
// reports it as the `uplink` block of `GET /api/v1/ground-station/status` and
// of `GET /api/v1/ground-station/uplink`; null while that service is not
// reporting, which every surface shows as unknown, never as a guess. The box's
// operator takes it back with `POST /api/v1/ground-station/uplink`.

import { apiFetch } from "../api-fetch";

export const UPLINK_PATH = "/api/v1/ground-station/uplink";

/** The wire block. */
export interface UplinkWire {
  state?: unknown;
  holder_device_id?: unknown;
  holder_name?: unknown;
  since_ms?: unknown;
  watcher?: { last_beacon_ms?: unknown } | null;
}

export interface UplinkView {
  state: "transmitting" | "standby";
  /** The phone holding the uplink: its name, else its device id. Null while
   *  the box transmits. */
  holder: string | null;
  /** When the box entered this state, epoch ms. */
  sinceMs: number | null;
  /** When the holder's presence beacon was last heard, epoch ms. */
  lastBeaconMs: number | null;
}

function text(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

function epochMs(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** The view of an `uplink` block, or null when absent or naming no known state. */
export function uplinkView(raw: unknown): UplinkView | null {
  if (typeof raw !== "object" || raw === null) return null;
  const u = raw as UplinkWire;
  if (u.state !== "transmitting" && u.state !== "standby") return null;
  const watcher = typeof u.watcher === "object" && u.watcher !== null ? u.watcher : null;
  return {
    state: u.state,
    holder: u.state === "standby" ? (text(u.holder_name) ?? text(u.holder_device_id)) : null,
    sinceMs: epochMs(u.since_ms),
    lastBeaconMs: epochMs(watcher?.last_beacon_ms),
  };
}

/** The refusal reason a box surface shows for a command while a phone holds
 *  the uplink, or null while the box transmits (or the state is unknown). */
export function uplinkHeldReason(view: UplinkView | null): string | null {
  return view?.state === "standby" ? `Uplink held by ${view.holder ?? "a phone"}` : null;
}

/** Take the uplink back: the holder is told to stop, then the box transmits. */
export function takeBackUplink(signal?: AbortSignal): Promise<{ ok: boolean; state?: string }> {
  return apiFetch(UPLINK_PATH, { method: "POST", body: { op: "take_back" }, signal });
}
