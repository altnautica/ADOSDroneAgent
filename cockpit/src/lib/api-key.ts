// Optional API-key persistence. On-box (the normal case) the cockpit runs
// from localhost, a trusted origin, and needs no credential. When the panel is
// reached off-box (a browser on another machine, a tunnel link), a key from the
// URL is captured once into localStorage and sent on requests + WS-ticket
// mints.
//
// # One credential per node, not one per page
//
// The dashboard and the cockpit are the same origin on the same agent, so a
// credential that works for one works for the other. They used to store it
// under different names, which meant signing in on the dashboard did nothing
// for the cockpit and the cockpit had no way of its own to obtain one: it never
// minted a session, and nothing in the product ever produced a link carrying a
// key to it. Reached from another machine it simply had no credential and every
// call was refused.
//
// So this reads and writes the SAME keys the dashboard does. Sign in once on
// the node and both surfaces work.
const STORAGE_KEY = "ados-api-key";
// Both spellings are accepted because both are already in circulation: the
// agent's own redirect preserves `?key=`, while this page historically read
// only `?ados_key=`, so a reach link built by the agent was silently ignored.
const URL_PARAMS = ["ados_key", "key"] as const;

function isBrowser(): boolean {
  return typeof window !== "undefined" && typeof localStorage !== "undefined";
}

/** The stored API key, or null when none is set. */
export function getApiKey(): string | null {
  if (!isBrowser()) return null;
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

/** Capture a one-shot key from the URL into storage, then strip it from the
 *  address bar so it is not left in history. Accepts either spelling; no-op
 *  when neither is present. */
export function consumeUrlKey(): void {
  if (!isBrowser()) return;
  try {
    const url = new URL(window.location.href);
    const param = URL_PARAMS.find((p) => url.searchParams.get(p));
    if (!param) return;
    const key = url.searchParams.get(param);
    if (!key) return;
    localStorage.setItem(STORAGE_KEY, key);
    // Strip every accepted spelling, not only the one that matched, so a link
    // carrying both does not leave one behind in history.
    for (const p of URL_PARAMS) url.searchParams.delete(p);
    window.history.replaceState(null, "", url.toString());
  } catch {
    // Storage disabled / malformed URL — ignore.
  }
}
