// REST wrappers + response types for the stable-MAC adapter surface. An onboard
// adapter with no efuse MAC randomizes its address each boot, which churns the
// node's DHCP lease and IP; the agent pins a stable MAC. These routes are native
// and profile-agnostic (they work on any node), so the MAC-pin page reads and
// writes them directly.

import { apiFetch } from "./api";

// GET /api/v1/network/mac/adapters — the per-adapter stable-MAC verdicts. An
// empty adapter list is a real fact ("nothing needs pinning"), distinct from a
// read failure.
export interface MacAdapter {
  name?: string | null;
  vidpid?: string | null;
  usbPath?: string | null;
  // "stable" | "pinned" | "candidate" | "deferred" | "disabled" (a forward state
  // renders raw).
  state?: string | null;
  appliedLive?: boolean;
  // "quirk" | "learned" | "override" (present only when known).
  source?: string;
  pinnedMac?: string;
  lastSeenMac?: string;
  linkFile?: string;
  deferredReason?: string;
}

export interface MacAdaptersView {
  version: number;
  adapters: MacAdapter[];
}

// POST /api/v1/network/mac/pin — pin a stable MAC. DELETE .../{iface} — unpin.
export interface MacPinResult {
  status: string;
  iface: string;
  mac?: string;
  persisted?: boolean;
  appliedLive?: boolean;
  removedOverride?: boolean;
  removedLinkFile?: boolean;
  note?: string;
}

export function getMacAdapters() {
  return apiFetch<MacAdaptersView>("/api/v1/network/mac/adapters");
}

// Pin a stable MAC on an adapter. An empty `mac` lets the agent use the
// learner's proposed value for a candidate. `apply_now` is intentionally omitted
// here (defaults false on the agent): the pin lands on the next boot and never
// re-tags the live interface, so it can't drop the link serving this dashboard.
export function pinMac(iface: string) {
  return apiFetch<MacPinResult>("/api/v1/network/mac/pin", {
    method: "POST",
    body: { iface },
  });
}

// Clear a pin: remove the override + the next-boot .link. Takes effect next
// boot; it does not re-tag the live interface.
export function unpinMac(iface: string) {
  return apiFetch<MacPinResult>(
    `/api/v1/network/mac/${encodeURIComponent(iface)}`,
    { method: "DELETE" },
  );
}
