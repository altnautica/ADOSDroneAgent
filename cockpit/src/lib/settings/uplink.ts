// The ground-station uplink section's logic: the leg matrix derived from the
// agent's aggregate network view, the failover-order draft, and the
// uplink-sharing change. Both writes can move the active uplink (and with it
// the operator's own path), so each is staged and only sent from a slide:
// `uplinkCommit` yields a request for a slide event and nothing otherwise.

import type { GsNetworkView, ShareUplinkResult } from "@/shared/api/uplink";

export const UPLINK_POLL_MS = 5_000;

export type LegKey = "ethernet" | "wifi" | "cellular" | "usb" | "ap";

/** The matrix row an uplink token names, or null for one with no row. */
export function legKey(token: string): LegKey | null {
  switch (token) {
    case "eth0":
    case "ethernet":
      return "ethernet";
    case "wlan0_client":
    case "wifi_client":
      return "wifi";
    case "wwan0":
    case "modem_4g":
      return "cellular";
    case "usb0":
    case "usb":
      return "usb";
    case "ap":
      return "ap";
    default:
      return null;
  }
}

/** The locale key labelling a token, or null (render the token raw). */
export function legLabelKey(token: string): `uplink.leg.${LegKey}` | null {
  const leg = legKey(token);
  return leg ? `uplink.leg.${leg}` : null;
}

/** The uplink locale keys this section's labels use. */
export type UplinkLabelKey =
  | "uplink.state.notReported"
  | "uplink.state.carryingTraffic"
  | "uplink.state.notConnected"
  | "uplink.state.connected"
  | "uplink.state.disabled"
  | "uplink.state.standingDown"
  | "uplink.state.broadcasting"
  | "uplink.state.off"
  | "uplink.toast.notApplied"
  | "uplink.toast.shareEnabled"
  | "uplink.toast.shareDisabled";

/** A label: a locale key with params, or raw agent text. A null param is a
 *  value the agent did not report (rendered "unknown"). */
export type Label = { key: UplinkLabelKey; params?: Record<string, string | null> } | { raw: string };

export interface LegState {
  leg: LegKey;
  state: Label;
  /** False renders the state muted ("not reported"). */
  known: boolean;
  detail: string | null;
  active: boolean;
}

const NOT_REPORTED: Label = { key: "uplink.state.notReported" };
const CARRYING: Label = { key: "uplink.state.carryingTraffic" };

/** The agent's own selected uplink token, never derived client-side. */
export function activeUplink(net: GsNetworkView | null): string | null {
  return typeof net?.active_uplink === "string" && net.active_uplink.length > 0 ? net.active_uplink : null;
}

/** One row per leg, in matrix order. Ethernet and USB have no live leg in the
 *  aggregate view, so they read "carrying traffic" only when the agent names
 *  them active, and "not reported" otherwise (never a false "no link"). */
export function legStates(net: GsNetworkView | null): LegState[] {
  const token = activeUplink(net);
  const active = token ? legKey(token) : null;

  const passive = (leg: LegKey): LegState =>
    active === leg
      ? { leg, state: CARRYING, known: true, detail: null, active: true }
      : { leg, state: NOT_REPORTED, known: false, detail: null, active: false };

  const wifi = net?.wifi_client ?? null;
  const wifiLeg: LegState = (() => {
    const base = { leg: "wifi" as const, active: active === "wifi" };
    if (!wifi || typeof wifi.connected !== "boolean") return { ...base, state: NOT_REPORTED, known: false, detail: null };
    if (!wifi.connected) return { ...base, state: { key: "uplink.state.notConnected" }, known: true, detail: null };
    return {
      ...base,
      state: { key: "uplink.state.connected" },
      known: true,
      detail: [wifi.ssid, typeof wifi.signal === "number" ? `${wifi.signal}%` : null, wifi.ip].filter(Boolean).join(" · ") || null,
    };
  })();

  const modem = net?.modem_4g ?? null;
  const cellularLeg: LegState = (() => {
    const base = { leg: "cellular" as const, active: active === "cellular" };
    if (!modem || typeof modem.enabled !== "boolean") return { ...base, state: NOT_REPORTED, known: false, detail: null };
    if (!modem.enabled) return { ...base, state: { key: "uplink.state.disabled" }, known: true, detail: null };
    const raw = typeof modem.state === "string" && modem.state.length > 0 ? modem.state : null;
    return {
      ...base,
      state: raw !== null ? { raw } : NOT_REPORTED,
      known: raw !== null,
      detail: typeof modem.percent === "number" ? `${modem.percent.toFixed(0)}%` : null,
    };
  })();

  const ap = net?.ap ?? null;
  const apLeg: LegState = (() => {
    const base = { leg: "ap" as const, active: active === "ap" };
    if (!ap || typeof ap.enabled !== "boolean") return { ...base, state: NOT_REPORTED, known: false, detail: null };
    if (ap.standing_down === true) {
      return {
        ...base,
        state: { key: "uplink.state.standingDown", params: { reason: ap.standdown_reason ?? null } },
        known: true,
        detail: ap.ssid ?? null,
      };
    }
    return ap.enabled
      ? { ...base, state: { key: "uplink.state.broadcasting" }, known: true, detail: ap.ssid ?? null }
      : { ...base, state: { key: "uplink.state.off" }, known: true, detail: null };
  })();

  return [passive("ethernet"), wifiLeg, cellularLeg, passive("usb"), apLeg];
}

/** Move `list[index]` by `delta`; null when out of range. */
export function moveEntry(list: readonly string[], index: number, delta: -1 | 1): string[] | null {
  const target = index + delta;
  if (index < 0 || index >= list.length) return null;
  if (target < 0 || target >= list.length) return null;
  const next = [...list];
  const [moved] = next.splice(index, 1);
  next.splice(target, 0, moved);
  return next;
}

/** The staged, not yet sent, changes. */
export interface UplinkUi {
  /** A reordered failover list; null when it matches the agent's. */
  draft: string[] | null;
  /** A share-uplink value awaiting its slide; null when none. */
  share: boolean | null;
}

export const UPLINK_UI_IDLE: UplinkUi = { draft: null, share: null };

export type UplinkAction =
  | { type: "move"; base: readonly string[]; index: number; delta: -1 | 1 }
  | { type: "revert" }
  | { type: "stageShare"; enabled: boolean }
  | { type: "cancelShare" }
  | { type: "prioritySaved" }
  | { type: "shareSaved" };

export function uplinkReducer(ui: UplinkUi, action: UplinkAction): UplinkUi {
  switch (action.type) {
    case "move": {
      const from = ui.draft ?? action.base;
      const next = moveEntry(from, action.index, action.delta);
      if (!next) return ui;
      const unchanged = next.length === action.base.length && next.every((v, i) => v === action.base[i]);
      return { ...ui, draft: unchanged ? null : next };
    }
    case "revert":
    case "prioritySaved":
      return { ...ui, draft: null };
    case "stageShare":
      return { ...ui, share: action.enabled };
    case "cancelShare":
    case "shareSaved":
      return { ...ui, share: null };
  }
}

export type UplinkRequest = { kind: "priority"; priority: string[] } | { kind: "share"; enabled: boolean };

/** The write a slide commits: only a slide yields a request, and only for a
 *  change that is staged. */
export function uplinkCommit(ui: UplinkUi, event: "prioritySlide" | "shareSlide"): UplinkRequest | null {
  if (event === "prioritySlide") return ui.draft ? { kind: "priority", priority: ui.draft } : null;
  return ui.share !== null ? { kind: "share", enabled: ui.share } : null;
}

/** How a share write landed: persisted and applied, or persisted but the
 *  daemon could not apply it to the live uplink. */
export function shareOutcome(res: ShareUplinkResult): {
  ok: boolean;
  key: UplinkLabelKey;
  params?: Record<string, string | null>;
} {
  if (res.applied === false) {
    return { ok: false, key: "uplink.toast.notApplied", params: { error: res.apply_error ?? null } };
  }
  return { ok: true, key: res.enabled ? "uplink.toast.shareEnabled" : "uplink.toast.shareDisabled" };
}
