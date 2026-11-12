// The ground station's IO reads and writes: game controllers (with the
// persisted primary, which drives RC input), the HDMI kiosk display, and
// Bluetooth discovery and pairing. Replies are coerced defensively.

import { apiFetch } from "@/shared/api-fetch";

const BASE = "/api/v1/ground-station";

/** How long a Bluetooth discovery scan listens, in seconds. */
export const BT_SCAN_SECONDS = 8;

export interface GamepadDevice {
  deviceId: string;
  name: string | null;
  path: string | null;
}

export interface Gamepads {
  devices: GamepadDevice[];
  primaryId: string | null;
}

export interface DisplayConfig {
  resolution: string | null;
  kioskEnabled: boolean;
  kioskTargetUrl: string | null;
}

export interface BtDevice {
  mac: string;
  name: string | null;
  type: string | null;
  connected: boolean;
}

type Obj = Record<string, unknown>;

function obj(v: unknown): Obj | null {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

export function parseGamepads(raw: unknown): Gamepads {
  const o = obj(raw);
  const devices: GamepadDevice[] = [];
  for (const item of Array.isArray(o?.devices) ? o.devices : []) {
    const d = obj(item);
    const deviceId = str(d?.device_id);
    if (!d || !deviceId) continue;
    devices.push({ deviceId, name: str(d.name), path: str(d.path) });
  }
  return { devices, primaryId: str(o?.primary_id) };
}

export function parseDisplay(raw: unknown): DisplayConfig | null {
  const o = obj(raw);
  if (!o) return null;
  return {
    resolution: str(o.resolution),
    kioskEnabled: o.kiosk_enabled === true,
    kioskTargetUrl: str(o.kiosk_target_url),
  };
}

export function parseBtDevices(raw: unknown): BtDevice[] {
  const list = obj(raw)?.devices;
  const out: BtDevice[] = [];
  for (const item of Array.isArray(list) ? list : []) {
    const d = obj(item);
    const mac = str(d?.mac);
    if (!d || !mac) continue;
    out.push({ mac, name: str(d.name), type: str(d.type), connected: d.connected === true });
  }
  return out;
}

/** Devices found by a scan that are not already paired (MAC compared
 *  case-insensitively). */
export function unpairedFound(found: readonly BtDevice[], paired: readonly BtDevice[]): BtDevice[] {
  const known = new Set(paired.map((d) => d.mac.toUpperCase()));
  return found.filter((d) => !known.has(d.mac.toUpperCase()));
}

export type PairOutcome = "connected" | "pairedNotConnected" | "failed";

/** Reads `POST …/bluetooth/pair` `{paired, connected?, error}`. */
export function pairOutcome(raw: unknown): { outcome: PairOutcome; error: string | null } {
  const o = obj(raw);
  const error = str(o?.error);
  if (o?.paired !== true) return { outcome: "failed", error };
  if (o.connected === false) return { outcome: "pairedNotConnected", error };
  return { outcome: "connected", error };
}

/** Whether a controller may be made primary: it must not already be, and no
 *  other write may be in flight. Making one primary moves RC input, so the
 *  screen fires it from a guarded button. */
export function canMakePrimary(pads: Gamepads, deviceId: string, busy: boolean, armedBlock: string | null): boolean {
  return !busy && armedBlock === null && pads.primaryId !== deviceId && pads.devices.some((d) => d.deviceId === deviceId);
}

export async function getGamepads(signal?: AbortSignal): Promise<Gamepads> {
  return parseGamepads(await apiFetch<unknown>(`${BASE}/gamepads`, { signal }));
}
export async function getDisplay(signal?: AbortSignal): Promise<DisplayConfig | null> {
  return parseDisplay(await apiFetch<unknown>(`${BASE}/display`, { signal }));
}
export async function getPairedBt(signal?: AbortSignal): Promise<BtDevice[]> {
  return parseBtDevices(await apiFetch<unknown>(`${BASE}/bluetooth/paired`, { signal }));
}
export async function scanBt(): Promise<BtDevice[]> {
  return parseBtDevices(
    await apiFetch<unknown>(`${BASE}/bluetooth/scan`, { method: "POST", body: { duration_s: BT_SCAN_SECONDS } }),
  );
}
export async function pairBt(mac: string): Promise<{ outcome: PairOutcome; error: string | null }> {
  return pairOutcome(await apiFetch<unknown>(`${BASE}/bluetooth/pair`, { method: "POST", body: { mac } }));
}
/** Forget (unpair) a Bluetooth device. */
export function forgetBt(mac: string): Promise<unknown> {
  return apiFetch(`${BASE}/bluetooth/${encodeURIComponent(mac)}`, { method: "DELETE" });
}
export function setPrimaryGamepad(deviceId: string): Promise<unknown> {
  return apiFetch(`${BASE}/gamepads/primary`, { method: "PUT", body: { device_id: deviceId } });
}
