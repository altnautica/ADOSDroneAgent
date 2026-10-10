// One-shot WebSocket ticket mint.
//
// A browser cannot set an `X-ADOS-Key` header on a WebSocket handshake, so the
// credential is exchanged for a one-shot ticket at `POST /api/_ws/ticket` and
// carried as a subprotocol value on the dial: `[WS_TICKET_PROTOCOL, ticket]`.
// On-box the mint needs no credential; off-box the stored session/key ride
// along. A failed mint returns null so the caller dials bare.

import { credentialHeaders } from "./api-fetch";

/** Subprotocol marker the agent expects before a presented ticket. */
export const WS_TICKET_PROTOCOL = "ados-ws-ticket";

export async function mintWsTicket(
  scope: string,
  signal?: AbortSignal,
): Promise<string | null> {
  try {
    const res = await fetch("/api/_ws/ticket", {
      method: "POST",
      headers: { "Content-Type": "application/json", ...credentialHeaders() },
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

/** The subprotocol list for a dial carrying `ticket` (empty when none). */
export function ticketProtocols(ticket: string | null): string[] {
  return ticket ? [WS_TICKET_PROTOCOL, ticket] : [];
}
