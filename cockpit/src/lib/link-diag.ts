// The one-glance WFB link diagnosis, mapped to the cockpit's semantic house
// grammar. The agent classifies WHY a link is or is not carrying data from its
// own decode counters (deaf / mis_keyed / jammed / healthy / searching); this
// turns that verdict into a colour, an icon, a short chip label, a full title,
// and an actionable-guidance hint. An absent / unknown verdict returns null so a
// surface omits the chip rather than fabricating a state — a status surface
// never invents a value it lacks.

import { KeyRound, Radar, SignalHigh, SignalZero, Zap, type LucideIcon } from "lucide-react";

import type { Tone } from "@/components/ui/data";

export interface LinkDiagView {
  /** The semantic colour for the verdict. */
  tone: Tone;
  /** A short chip label for tight surfaces (the top bar). */
  label: string;
  /** A fuller sentence-case title for the warning banner + Link screen. */
  title: string;
  /** The icon that reads at a glance. */
  icon: LucideIcon;
  /** One line of what to do about it. Present only for actionable verdicts. */
  hint?: string;
  /** True when the operator must act — the Feed shows a centre warning. */
  actionable: boolean;
}

/** Map the agent's `link_diag` verdict to its cockpit view, or `null` when the
 *  agent has not classified the link (the field is null / absent / unknown), so
 *  the caller omits the chip instead of showing a fabricated state. */
export function linkDiagView(diag: string | null | undefined): LinkDiagView | null {
  switch (diag) {
    case "healthy":
      return { tone: "ok", label: "Link up", title: "Link up", icon: SignalHigh, actionable: false };
    case "searching":
      return { tone: "muted", label: "Searching", title: "Searching for the link", icon: Radar, actionable: false };
    case "deaf":
      return {
        tone: "err",
        label: "No RF",
        title: "No RF reaching the receiver",
        icon: SignalZero,
        hint: "The receiver hears nothing — check the drone radio, antenna, and channel",
        actionable: true,
      };
    case "mis_keyed":
      return {
        tone: "err",
        label: "Key mismatch",
        title: "Link key mismatch",
        icon: KeyRound,
        hint: "RF is arriving but cannot be decoded — re-pair the drone to this ground station",
        actionable: true,
      };
    case "jammed":
      return {
        tone: "warn",
        label: "Interference",
        title: "Interference on the link",
        icon: Zap,
        hint: "Signal too weak or noisy to decode — reduce range or clear obstructions",
        actionable: true,
      };
    default:
      return null;
  }
}
