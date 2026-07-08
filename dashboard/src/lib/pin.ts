// Client calls for the dashboard-access PIN gate.
//
// These hit the agent's public PIN endpoints directly (not through `apiFetch`):
// the whole point of the gate is that the browser has no data-plane credential
// yet, so these calls must not send/clear a session or trip the access-gate
// signal. On a correct PIN the agent returns a session token, stored via
// `setSession` so subsequent `apiFetch` calls carry it.

import { setSession } from "./session";

export interface PinStatus {
  pin_set: boolean;
  locked: boolean;
  locked_until: number | null;
}

export interface NodeIdentity {
  name: string;
  profile: string;
}

const JSON_HEADERS = { Accept: "application/json" } as const;

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
    if (!res.ok) return { name: "ADOS node", profile: "" };
    const body = (await res.json()) as { name?: string; profile?: string };
    return { name: body.name?.trim() || "ADOS node", profile: body.profile ?? "" };
  } catch {
    return { name: "ADOS node", profile: "" };
  }
}

export type PinSubmitResult =
  | { ok: true }
  | { ok: false; kind: "wrong"; remaining: number }
  | { ok: false; kind: "locked"; lockedUntil: number }
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
    return { ok: false, kind: "error", message: "Network error. Is the node reachable?" };
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
  if (res.status === 429) {
    return {
      ok: false,
      kind: "locked",
      lockedUntil: typeof data?.locked_until === "number" ? data.locked_until : 0,
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
      message: typeof data?.detail === "string" ? data.detail : "That PIN was not accepted.",
    };
  }
  return {
    ok: false,
    kind: "error",
    message: typeof data?.detail === "string" ? data.detail : `Request failed (${res.status}).`,
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
