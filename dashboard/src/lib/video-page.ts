// What the Video page and panel show for each profile.

import type { Profile } from "@/shared/api/types";

/** The long-range radio cards (Radio, Link tuning) belong to the nodes that
 *  carry a WFB radio: a visor has none. */
export function showsLongRangeRadio(profile: Profile): boolean {
  return profile === "drone" || profile === "ground_station";
}

/** The camera assignments as rows (`main → /dev/video0`), sorted by role. */
export function assignmentRows(assignments: Record<string, string> | null | undefined): Array<{ role: string; device: string }> {
  return Object.entries(assignments ?? {})
    .filter(([, device]) => typeof device === "string" && device.length > 0)
    .map(([role, device]) => ({ role, device }))
    .sort((a, b) => a.role.localeCompare(b.role));
}

/** The second line of the idle video empty state. A ground station has no
 *  camera of its own: it waits for the drone's stream. */
export function videoIdleDetailKey(isGround: boolean): "video.panel.gsIdle" | "video.panel.plugInCamera" {
  return isGround ? "video.panel.gsIdle" : "video.panel.plugInCamera";
}
