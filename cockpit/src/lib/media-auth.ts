import { getApiKey } from "./api-key";
import { getSession } from "./session";

/**
 * The data-plane credential headers for a media request.
 *
 * The video paths (`/whep`, `/hls`) sit outside `/api/`, and the agent's proxy
 * currently exempts everything outside `/api/` from its credential check — so a
 * PAIRED node serves its live video to any peer on the LAN with no credential
 * at all. That exemption is load-bearing precisely because these clients have
 * never sent one: closing it without wiring them first would black out the
 * ground station's own video.
 *
 * So these headers are sent BEFORE the gate narrows. They are inert until then
 * — the agent ignores an unrecognised header — which is what makes the eventual
 * flip a one-line change with nothing left to discover.
 *
 * Same two credentials `apiFetch` already sends: the dashboard session minted by
 * the PIN gate, and the stored API key from the Mission Control deep link.
 */
export function mediaAuthHeaders(): Record<string, string> {
  const headers: Record<string, string> = {};
  const session = getSession();
  if (session) headers["X-ADOS-Dashboard-Session"] = session;
  const key = getApiKey();
  if (key) headers["X-ADOS-Key"] = key;
  return headers;
}
