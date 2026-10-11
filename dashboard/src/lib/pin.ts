// Client calls for the dashboard-access PIN gate.
//
// These hit the agent's public PIN endpoints directly (not through `apiFetch`):
// the whole point of the gate is that the browser has no data-plane credential
// yet, so these calls must not send/clear a session or trip the access-gate
// signal. On a correct PIN the agent returns a session token, stored via
// `setSession` so subsequent `apiFetch` calls carry it.

import i18n from "@/i18n";
import { setSession } from "@/shared/session";

export type LockScope = "peer" | "node";

export interface PinStatus {
  pin_set: boolean;
  locked: boolean;
  locked_until: number | null;
  /** `node` when the node-wide failure budget tripped (every caller is locked
   *  out); `peer` for this caller's own lockout; null when not locked. */
  scope?: LockScope | null;
}

/** What the splash shows for a PIN status. A null status (the read failed) is
 *  `unknown`, never `set`: offering to set a PIN on a node that may already
 *  have one would fail confusingly on submit. */
export type SplashMode = "unknown" | "set" | "enter" | "locked";

export function splashMode(status: PinStatus | null): SplashMode {
  if (!status) return "unknown";
  if (status.locked) return "locked";
  return status.pin_set ? "enter" : "set";
}

/** The locked-state message for a lockout scope and the seconds remaining. */
export function lockMessage(scope: LockScope, remainingS: number): string {
  if (scope === "node") {
    const min = Math.max(1, Math.ceil(remainingS / 60));
    return i18n.t("access.pin.nodeLocked", { min });
  }
  return i18n.t("access.pin.peerLocked");
}

export interface NodeIdentity {
  name: string;
  profile: string;
}

const JSON_HEADERS = { Accept: "application/json" } as const;

/** The agent's PIN rule (crates/ados-control dashboard_pin): 4-12 ASCII digits. */
export const PIN_MIN_LENGTH = 4;
export const PIN_MAX_LENGTH = 12;

export function isValidPin(pin: string): boolean {
  return pin.length >= PIN_MIN_LENGTH && pin.length <= PIN_MAX_LENGTH && /^\d+$/.test(pin);
}

/** `GET /api/dashboard/pin/status` — public; picks the splash mode. */
export async function fetchPinStatus(signal?: AbortSignal): Promise<PinStatus> {
  const res = await fetch("/api/dashboard/pin/status", {
    headers: JSON_HEADERS,
    signal,
  });
  if (!res.ok) throw new Error(`pin status ${res.status}`);
  return (await res.json()) as PinStatus;
}

/** Node name + profile for the splash's identity line. `/api/pairing/info` is
 * public, so it answers even while the data plane is locked. */
export async function fetchNodeIdentity(signal?: AbortSignal): Promise<NodeIdentity> {
  try {
    const res = await fetch("/api/pairing/info", { headers: JSON_HEADERS, signal });
    if (!res.ok) return { name: i18n.t("access.pin.fallbackNodeName"), profile: "" };
    const body = (await res.json()) as { name?: string; profile?: string };
    return { name: body.name?.trim() || i18n.t("access.pin.fallbackNodeName"), profile: body.profile ?? "" };
  } catch {
    return { name: i18n.t("access.pin.fallbackNodeName"), profile: "" };
  }
}

export type PinSubmitResult =
  | { ok: true }
  | { ok: false; kind: "wrong"; remaining: number }
  | { ok: false; kind: "locked"; lockedUntil: number; scope: LockScope }
  | { ok: false; kind: "invalid"; message: string }
  | { ok: false; kind: "error"; message: string };

async function submit(path: string, body: object): Promise<PinSubmitResult> {
  let res: Response;
  try {
    res = await fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json", ...JSON_HEADERS },
      body: JSON.stringify(body),
    });
  } catch {
    return { ok: false, kind: "error", message: i18n.t("access.pin.networkError") };
  }
  let data: Record<string, unknown> | null = null;
  try {
    data = (await res.json()) as Record<string, unknown>;
  } catch {
    // Non-JSON body; fall through to the status-code mapping.
  }
  if (res.ok && typeof data?.session === "string") {
    setSession(data.session, typeof data.expires_at === "number" ? data.expires_at : 0);
    return { ok: true };
  }
  if (res.status === 423) {
    // The node-wide failure budget tripped: every caller is locked out.
    const retryS = typeof data?.retry_after_s === "number" ? data.retry_after_s : 0;
    return { ok: false, kind: "locked", lockedUntil: Date.now() / 1000 + retryS, scope: "node" };
  }
  if (res.status === 429) {
    return {
      ok: false,
      kind: "locked",
      lockedUntil: typeof data?.locked_until === "number" ? data.locked_until : 0,
      scope: data?.scope === "node" ? "node" : "peer",
    };
  }
  if (res.status === 401) {
    return {
      ok: false,
      kind: "wrong",
      remaining: typeof data?.remaining_attempts === "number" ? data.remaining_attempts : 0,
    };
  }
  if (res.status === 400 || res.status === 403 || res.status === 409) {
    return {
      ok: false,
      kind: "invalid",
      message: typeof data?.detail === "string" ? data.detail : i18n.t("access.pin.notAccepted"),
    };
  }
  return {
    ok: false,
    kind: "error",
    message: typeof data?.detail === "string" ? data.detail : i18n.t("access.pin.requestFailed", { status: res.status }),
  };
}

/** Set (or, with `currentPin`, change) the dashboard PIN. */
export function setDashboardPin(pin: string, currentPin?: string): Promise<PinSubmitResult> {
  return submit("/api/dashboard/pin/set", currentPin ? { pin, current_pin: currentPin } : { pin });
}

/** Enter the dashboard PIN. */
export function verifyDashboardPin(pin: string): Promise<PinSubmitResult> {
  return submit("/api/dashboard/pin/verify", { pin });
}
