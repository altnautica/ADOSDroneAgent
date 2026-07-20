// One-shot WebSocket ticket mint.
//
// A browser cannot set an `X-ADOS-Key` header on a WebSocket handshake, so the
// pairing credential is exchanged for a one-shot HMAC ticket at
// `POST /api/_ws/ticket` and carried as a subprotocol value on the dial. On-box
// (the panel's normal case) the mint is unguarded and needs no credential; when
// reached off-box the optional session/key are attached. A mint that fails
// returns null so the caller dials bare (the open on-box posture).
//
// The agent expects the offered subprotocols to be `[<marker>, <ticket>]` and
// echoes the marker back per RFC 6455 (see the native `gs_ws` handlers).

import { getApiKey } from "@/lib/api-key";
import { getSession } from "@/lib/session";

/** Subprotocol marker the agent expects before a presented ticket. */
export const WS_TICKET_PROTOCOL = "ados-ws-ticket";

/** Mint a ticket for `scope`, or null when none can be minted (mint failure /
 *  open posture) so the caller dials bare. */
export async function mintWsTicket(
  scope: string,
  signal?: AbortSignal,
): Promise<string | null> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const key = getApiKey();
  if (key) headers["X-ADOS-Key"] = key;

  try {
    const res = await fetch("/api/_ws/ticket", {
      method: "POST",
      headers,
      body: JSON.stringify({ scope }),
      signal,
    });
    if (!res.ok) return null;
    const body = (await res.json()) as { ticket?: string };
    return body.ticket ?? null;
  } catch {
    return null;
  }
}
