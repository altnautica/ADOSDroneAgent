// The MAC-pin section's logic: which action an adapter offers, and how a pin or
// unpin landed. The actions are hold-guarded on the page; `runMacPinAction` is
// what the guard fires, so building the action list never touches the agent.

import type { MacAdapter, MacPinResult } from "@/shared/api/mac-pin";

export const MAC_ADAPTERS_POLL_MS = 15_000;

export interface MacPinAction {
  kind: "pin" | "unpin";
  iface: string;
}

/** A candidate can be pinned, a pinned adapter unpinned; nothing else offers
 *  an action. An adapter with no interface name cannot be addressed. */
export function adapterAction(adapter: MacAdapter): MacPinAction | null {
  const iface = adapter.name ?? null;
  if (!iface) return null;
  const state = adapter.state ?? "stable";
  if (state === "candidate") return { kind: "pin", iface };
  if (state === "pinned") return { kind: "unpin", iface };
  return null;
}

/** The badge tone for an adapter state; a forward state renders neutral. */
export function adapterTone(state: string): "ok" | "warn" | "muted" {
  if (state === "pinned") return "ok";
  if (state === "candidate" || state === "deferred") return "warn";
  return "muted";
}

export type MacPinOutcome =
  | { ok: true; note: string | null; key: "settingsMore.macPin.unpinned" | "settingsMore.macPin.pinned" }
  | { ok: false; key: "settingsMore.macPin.stillPinned"; detail: string };

/** How a write landed. An unpin answers 200 even when half of it failed (the
 *  config write refused, or the next-boot .link not deleted): the adapter is
 *  then still pinned at the next boot. */
export function macPinOutcome(action: MacPinAction, res: MacPinResult): MacPinOutcome {
  if (action.kind === "unpin") {
    const failure = res.link_error ?? res.persist_error;
    if (failure) return { ok: false, key: "settingsMore.macPin.stillPinned", detail: failure };
    return { ok: true, note: res.note || null, key: "settingsMore.macPin.unpinned" };
  }
  return { ok: true, note: res.note || null, key: "settingsMore.macPin.pinned" };
}

export interface MacPinApi {
  pinMac: (iface: string) => Promise<MacPinResult>;
  unpinMac: (iface: string) => Promise<MacPinResult>;
}

/** Send the action (the guard's `onFired`). */
export async function runMacPinAction(action: MacPinAction, api: MacPinApi): Promise<MacPinOutcome> {
  const res = action.kind === "pin" ? await api.pinMac(action.iface) : await api.unpinMac(action.iface);
  return macPinOutcome(action, res);
}
