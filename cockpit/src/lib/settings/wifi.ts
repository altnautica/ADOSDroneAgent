// The Wi-Fi client section's logic: the scan list merge, the join target, and
// the join flow. A join is a password sheet whose slide sends the request; an
// agent refusing it with a 409 `needs_force` (the node's own access point holds
// the radio) re-prompts with a second slide that repeats the join with `force`.
// The request is only ever produced from a `joining` phase, and the only ways
// into that phase are the two slides.

import { ApiError } from "@/shared/api-fetch";
import type { WifiJoinResult, WifiNetwork } from "@/shared/api/types";
import { isSecured } from "@/shared/api/wifi";

export const WIFI_STATUS_POLL_MS = 5_000;
export const WIFI_SAVED_POLL_MS = 15_000;
/** The SSID length limit (802.11). */
export const SSID_MAX = 32;
/** The WPA passphrase length limit. */
export const PASSPHRASE_MAX = 63;

/** Collapse the per-BSSID rows nmcli emits for one SSID into the strongest,
 *  carrying `in_use` if any duplicate is the connected BSSID. Hidden networks
 *  (empty SSID) pass through. Sorted strongest first. */
export function mergeNetworks(all: readonly WifiNetwork[]): WifiNetwork[] {
  const grouped = new Map<string, WifiNetwork>();
  const hidden: WifiNetwork[] = [];
  for (const net of all) {
    if (!net.ssid) {
      hidden.push(net);
      continue;
    }
    const prior = grouped.get(net.ssid);
    if (!prior) {
      grouped.set(net.ssid, net);
      continue;
    }
    const stronger = net.signal > prior.signal ? net : prior;
    grouped.set(net.ssid, { ...stronger, in_use: prior.in_use || net.in_use });
  }
  return [...grouped.values(), ...hidden].sort((a, b) => b.signal - a.signal);
}

export interface JoinTarget {
  ssid: string;
  security: string;
  /** A saved profile exists, so a blank password reuses its credentials. */
  saved: boolean;
}

export function targetForNetwork(net: WifiNetwork, savedNames: ReadonlySet<string>): JoinTarget {
  return { ssid: net.ssid, security: net.security, saved: savedNames.has(net.ssid) };
}

/** A typed hidden SSID, assumed secured; null when blank. */
export function manualTarget(raw: string, savedNames: ReadonlySet<string>): JoinTarget | null {
  const ssid = raw.trim();
  if (!ssid) return null;
  return { ssid, security: "WPA2", saved: savedNames.has(ssid) };
}

/** An inline error on the join sheet: a locale key or the agent's own text. */
export type JoinError = { key: "wifi.join.passwordRequired" | "wifi.join.failed" } | { text: string };

export type JoinState =
  | { phase: "closed" }
  | { phase: "password"; target: JoinTarget; password: string; error: JoinError | null }
  | { phase: "joining"; target: JoinTarget; password: string; force: boolean }
  | { phase: "force"; target: JoinTarget; password: string; message: string | null };

export type JoinAction =
  | { type: "open"; target: JoinTarget }
  | { type: "password"; value: string }
  /** The join slide on the password sheet. */
  | { type: "slide" }
  /** The second slide, after the agent asked for `force`. */
  | { type: "forceSlide" }
  | { type: "result"; result: WifiJoinResult }
  | { type: "failed"; err: unknown }
  | { type: "close" };

export const JOIN_CLOSED: JoinState = { phase: "closed" };

/** The agent's "the access point holds the radio" refusal, with its hint. The
 *  native agent puts `needs_force` at the top level; the older shape nested it
 *  under `detail`. */
export function needsForce(err: unknown): { message: string | null } | null {
  if (!(err instanceof ApiError) || err.status !== 409) return null;
  const body = (err.body ?? null) as {
    needs_force?: unknown;
    detail?: { needs_force?: unknown; error?: { message?: unknown } };
  } | null;
  const flagged =
    body?.needs_force === true || body?.detail?.needs_force === true || err.code === "E_WLAN0_BUSY_AP_ACTIVE";
  if (!flagged) return null;
  const msg = body?.detail?.error?.message;
  return { message: typeof msg === "string" && msg ? msg : null };
}

export function joinReducer(state: JoinState, action: JoinAction): JoinState {
  switch (action.type) {
    case "open":
      return { phase: "password", target: action.target, password: "", error: null };
    case "close":
      // A request in flight finishes on its own; the sheet stays until it does.
      return state.phase === "joining" ? state : JOIN_CLOSED;
    case "password":
      return state.phase === "password" ? { ...state, password: action.value, error: null } : state;
    case "slide": {
      if (state.phase !== "password") return state;
      const { target, password } = state;
      if (isSecured(target.security) && !target.saved && !password) {
        return { ...state, error: { key: "wifi.join.passwordRequired" } };
      }
      return { phase: "joining", target, password, force: false };
    }
    case "forceSlide":
      return state.phase === "force"
        ? { phase: "joining", target: state.target, password: state.password, force: true }
        : state;
    case "result": {
      if (state.phase !== "joining") return state;
      if (action.result.joined) return JOIN_CLOSED;
      const error: JoinError = action.result.error ? { text: action.result.error } : { key: "wifi.join.failed" };
      return { phase: "password", target: state.target, password: state.password, error };
    }
    case "failed": {
      if (state.phase !== "joining") return state;
      const conflict = state.force ? null : needsForce(action.err);
      if (conflict) {
        return { phase: "force", target: state.target, password: state.password, message: conflict.message };
      }
      // Any other failure is toasted; the sheet stays open for a retry.
      return { phase: "password", target: state.target, password: state.password, error: null };
    }
  }
}

export interface JoinRequest {
  ssid: string;
  passphrase: string | null;
  force: boolean;
}

/** The join to send, only from the `joining` phase. An open network sends no
 *  passphrase; a blank one on a saved profile reuses the saved credentials. */
export function joinRequest(state: JoinState): JoinRequest | null {
  if (state.phase !== "joining") return null;
  const secured = isSecured(state.target.security);
  return {
    ssid: state.target.ssid,
    passphrase: secured ? state.password || null : null,
    force: state.force,
  };
}
