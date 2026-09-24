import { getSession } from "./session";

// The video paths (`/whep`, `/hls`) are credential-gated on a paired node: the
// agent's front admits them only on the pairing key or a dashboard session, and
// the media server behind it binds loopback so there is no other way in. Code
// that fetches them itself sends `credentialHeaders()` (lib/api); this module
// covers the one case that cannot carry a header.

/** The query parameter the agent accepts a session under, media plane only. */
const SESSION_QUERY_KEY = "ados_session";

/**
 * Same session, carried in the URL instead of a header.
 *
 * A `<video>` element (native HLS on Safari/iOS) issues its own requests for a
 * playlist and its segments, and there is no hook to attach a header to them,
 * so the agent accepts the session as a query parameter on `/whep` and `/hls`
 * and nowhere else.
 *
 * Use this ONLY for a URL a media element will fetch on its own. Anything this
 * code fetches itself sends the header instead: a credential in a URL lands in
 * access logs, browser history and `Referer`.
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
