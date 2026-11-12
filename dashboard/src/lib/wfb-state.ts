import type { BadgeVariant } from "@/components/ui/badge";
import { wfbStateLabel } from "@/lib/state-labels";

/** The WFB state badge, labelled through `wfbStateLabel`. `stale` is a known state: a warning, with the
 *  panel's values dimmed (`wfbStale`). */
export function wfbStateBadge(state: string): { label: string; variant: BadgeVariant } | null {
  switch (state) {
    case "active":
    case "ready":
      return { label: wfbStateLabel(state), variant: "success" };
    case "connecting":
      return { label: wfbStateLabel(state), variant: "info" };
    case "stale":
      return { label: wfbStateLabel(state), variant: "warning" };
    case "error":
      return { label: wfbStateLabel(state), variant: "warning" };
    case "disabled":
      return { label: wfbStateLabel(state), variant: "neutral" };
    default:
      return state && state !== "unknown" ? { label: wfbStateLabel(state), variant: "neutral" } : null;
  }
}

/** Values from a stale link read are dimmed and toned neutral. */
export function wfbStale(state: string): boolean {
  return state === "stale";
}
