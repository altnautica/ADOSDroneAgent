// The owner this cockpit window names on its vision leases (depth and
// surfaces demand, detection demand). The agent keeps one lease per owner, so
// every window needs its own: two windows that shared `cockpit` would drop
// each other's lease when one of them released it (closing a view sends
// hold 0). One id per page load.

/** `cockpit:<8 hex>` from a random source. */
export function makeLeaseOwner(random: () => number = Math.random): string {
  let hex = "";
  for (let i = 0; i < 8; i++) hex += Math.floor(random() * 16).toString(16);
  return `cockpit:${hex}`;
}

/** This window's lease owner, fixed for the page's lifetime. */
export const LEASE_OWNER: string = makeLeaseOwner();
