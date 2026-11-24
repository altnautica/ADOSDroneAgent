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
    case "searching":
      return { label: wfbStateLabel(state), variant: "info" };
    case "deaf":
    case "no_peer":
      return { label: wfbStateLabel(state), variant: "warning" };
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

/** Verdicts under which this radio decoded nothing. */
const UNMEASURED_DIAG: Record<string, true> = { deaf: true, no_rf: true, no_peer: true };

/** Whether loss, SNR and rate describe real decoded traffic. With nothing
 *  decoded they are not measured and render as a dash, never 0.0% loss. */
export function wfbMeasured(w: { packets_received?: number | null; link_diag?: string | null } | null | undefined): boolean {
  if (!w) return false;
  if (w.link_diag && UNMEASURED_DIAG[w.link_diag]) return false;
  return typeof w.packets_received === "number" && w.packets_received > 0;
}
