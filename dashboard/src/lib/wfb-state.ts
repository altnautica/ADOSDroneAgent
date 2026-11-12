import type { TFunction } from "i18next";

import type { BadgeVariant } from "@/components/ui/badge";

/** The WFB state badge. `stale` is a known state: a warning, with the
 *  panel's values dimmed (`wfbStale`). */
export function wfbStateBadge(state: string, t: TFunction): { label: string; variant: BadgeVariant } | null {
  switch (state) {
    case "active":
    case "ready":
      return { label: state, variant: "success" };
    case "connecting":
      return { label: t("wfb.state.connecting"), variant: "info" };
    case "stale":
      return { label: t("wfb.state.stale"), variant: "warning" };
    case "error":
      return { label: t("wfb.state.error"), variant: "warning" };
    case "disabled":
      return { label: t("wfb.state.disabled"), variant: "neutral" };
    default:
      return state && state !== "unknown" ? { label: state, variant: "neutral" } : null;
  }
}

/** Values from a stale link read are dimmed and toned neutral. */
export function wfbStale(state: string): boolean {
  return state === "stale";
}
