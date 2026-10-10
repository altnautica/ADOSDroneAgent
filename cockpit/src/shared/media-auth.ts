// The video paths (`/whep`, `/hls`) are credential-gated on a paired node.
// Code that fetches them itself sends `credentialHeaders()`; this module covers
// the one case that cannot carry a header.

import { getSession } from "./session";

/** The query parameter the agent accepts a session under, media plane only. */
const SESSION_QUERY_KEY = "ados_session";

/**
 * The same session, carried in the URL. A `<video>` element (native HLS on
 * Safari/iOS) issues its own requests and offers no hook for a header, so the
 * agent accepts the session as a query parameter on `/whep` and `/hls` only.
 * Use this ONLY for a URL a media element fetches on its own: a credential in a
 * URL lands in logs, history and `Referer`. Returns the URL unchanged when no
 * session is stored.
 */
export function withMediaAuth(url: string): string {
  const session = getSession();
  if (!session) return url;
  const sep = url.includes("?") ? "&" : "?";
  return `${url}${sep}${SESSION_QUERY_KEY}=${encodeURIComponent(session)}`;
}
