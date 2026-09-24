import { getApiKey } from "./api-key";
import { getSession } from "./session";

/**
 * The data-plane credential headers for a media request.
 *
 * The video paths (`/whep`, `/hls`) are credential-gated: off-box, the agent's
 * front admits them only on the pairing key or a dashboard session, and the
 * media server behind it binds loopback so there is no other way in.
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

/** The query parameter the agent accepts a session under, media plane only. */
const SESSION_QUERY_KEY = "ados_session";

/**
 * Same session, carried in the URL instead of a header.
 *
 * A `<video>` element issues its own requests for a playlist and its segments,
 * and there is no hook to attach a header to them — so a header-only credential
 * is simply unreachable for element-driven playback, and the operator gets a
 * black frame with no way to authenticate it. The agent therefore accepts the
 * session as a query parameter on `/whep` and `/hls` and nowhere else.
 *
 * Use this ONLY for a URL a media element or player library will fetch on its
 * own. Anything this code fetches itself should send the header instead: a
 * credential in a URL lands in access logs, browser history and `Referer`, and
 * that cost is only worth paying where there is no alternative.
 *
 * Returns the URL unchanged when there is no session, so an on-box viewer (which
 * needs no credential) is not handed an empty parameter.
 */
export function withMediaAuth(url: string): string {
  const session = getSession();
  if (!session) return url;
  const sep = url.includes("?") ? "&" : "?";
  return `${url}${sep}${SESSION_QUERY_KEY}=${encodeURIComponent(session)}`;
}
