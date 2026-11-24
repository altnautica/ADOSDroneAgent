// Pure label/tone mapping for the Radio link screen, kept out of the component
// so it is unit-testable. Covers every state the drone radio daemon and the
// ground-station receiver write to `GET /api/wfb`.

import type { Tone } from "@/lib/tone";

/** Tone per link state. The receiver's `active` means it is decoding, the
 *  same as the drone's `connected`. */
const LINK_TONES: Record<string, Tone> = {
  connected: "ok",
  active: "ok",
  degraded: "warn",
  rf_unverified: "warn",
  connecting: "muted",
  searching: "muted",
  binding: "muted",
  auto_pairing: "muted",
  unpaired: "muted",
};

export function linkTone(state: string | undefined, active: boolean | undefined): Tone {
  if (state && state in LINK_TONES) return LINK_TONES[state];
  return active ? "warn" : "err";
}

/** The link state's label key; an unlisted state reads as unknown. */
const LINK_STATE_KEYS = {
  connected: "cockpit.link.state.connected",
  active: "cockpit.link.state.connected",
  degraded: "cockpit.link.state.degraded",
  rf_unverified: "cockpit.link.state.rf_unverified",
  connecting: "cockpit.link.state.connecting",
  searching: "cockpit.link.state.searching",
  binding: "cockpit.link.state.binding",
  auto_pairing: "cockpit.link.state.auto_pairing",
  unpaired: "cockpit.link.state.unpaired",
  disconnected: "cockpit.link.state.disconnected",
  stopped: "cockpit.link.state.stopped",
  stale: "cockpit.link.state.stale",
} as const;

export type LinkStateKey = (typeof LINK_STATE_KEYS)[keyof typeof LINK_STATE_KEYS] | "cockpit.link.state.unknown";

export function linkStateKey(state: string): LinkStateKey {
  return state in LINK_STATE_KEYS ? LINK_STATE_KEYS[state as keyof typeof LINK_STATE_KEYS] : "cockpit.link.state.unknown";
}

/** The regulatory domain as one name everywhere: the raw `global` token is the
 *  "Unrestricted" setting in Settings › Region. Null when not reported. */
export function regionLabelKey(domain: string | null | undefined): "cockpit.link.regionUnrestricted" | null {
  return domain != null && domain.toLowerCase() === "global" ? "cockpit.link.regionUnrestricted" : null;
}
