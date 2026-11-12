// What the Video page and panel show for each profile.

import { DEFAULT_CAMERA_ID } from "@/lib/video-layer-lease";
import type { Profile } from "@/shared/api/types";
import { whepUrlFor } from "@/shared/whep";

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

/** The WHEP and HLS endpoints for the chosen camera. The primary camera uses
 *  the URLs the node advertises; another leg is `?camera=<id>` on the WHEP
 *  endpoint and `/hls/<id>/index.m3u8`. A node advertising no video offers
 *  no leg at all. */
export function cameraVideoUrls(
  primary: { whepUrl: string; hlsUrl: string },
  cameraId: string,
): { whepUrl: string; hlsUrl: string } {
  if (cameraId === DEFAULT_CAMERA_ID) return primary;
  return {
    whepUrl: primary.whepUrl ? whepUrlFor(primary.whepUrl, cameraId) : "",
    hlsUrl: primary.hlsUrl ? `/hls/${encodeURIComponent(cameraId)}/index.m3u8` : "",
  };
}
