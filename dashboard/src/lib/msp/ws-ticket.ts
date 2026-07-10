/**
 * One-shot WebSocket ticket mint for the MAVLink/MSP proxy.
 *
 * A browser cannot set `X-ADOS-Key` on a WebSocket handshake, so the pairing
 * credential is exchanged for a one-shot ticket at `POST /api/_ws/ticket` and
 * carried as a subprotocol value on the dial. We call `fetch` directly (rather
 * than the shared `apiFetch`) so a mint that 401s never tears down the
 * dashboard session — a missing ticket just falls through to the open-posture
 * bare dial.
 *
 * @module lib/msp/ws-ticket
 * @license GPL-3.0-only
 */

import { getApiKey } from "@/lib/api-key";
import { getSession } from "@/lib/session";

/** Subprotocol marker the agent expects before a presented ticket. */
export const WS_TICKET_PROTOCOL = "ados-ws-ticket";

/** Mint a `gs.mavlink_ws` ticket, or null when none can be minted (unpaired /
 *  open posture / mint failure) so the caller dials bare. */
export async function mintMavlinkWsTicket(signal?: AbortSignal): Promise<string | null> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const key = getApiKey();
  if (key) headers["X-ADOS-Key"] = key;

  try {
    const res = await fetch("/api/_ws/ticket", {
      method: "POST",
      headers,
      body: JSON.stringify({ scope: "gs.mavlink_ws" }),
      signal,
    });
    if (!res.ok) return null;
    const body = (await res.json()) as { ticket?: string };
    return body.ticket ?? null;
  } catch {
    return null;
  }
}
