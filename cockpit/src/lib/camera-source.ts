// The Settings leaf for `video.camera.source`: a picker over the cameras the
// agent detected (`GET /api/video/cameras`), with free text kept as "Other…"
// for a device the scan does not list.

import type { VideoCamera } from "@/lib/video-model";

export const CAMERA_SOURCE_PATH = "video.camera.source";

/** The picker's sentinel for "type a device path". Not a legal path. */
export const OTHER_SOURCE = "\u0000other";

export interface CameraSourceOption {
  value: string;
  label: string;
}

/** One option per detected camera with a device path, labelled with its name
 *  and path, de-duplicated by path, in the order the agent listed them. */
export function cameraSourceOptions(cameras: readonly VideoCamera[]): CameraSourceOption[] {
  const seen = new Set<string>();
  const out: CameraSourceOption[] = [];
  for (const c of cameras) {
    const path = c.devicePath.trim();
    if (!path || seen.has(path)) continue;
    seen.add(path);
    const name = c.label.trim();
    out.push({ value: path, label: name && name !== path ? `${name} · ${path}` : path });
  }
  return out;
}

/** Whether the pending value is free text (not one of the detected paths). */
export function isOtherSource(pending: string, options: readonly CameraSourceOption[]): boolean {
  return pending !== "" && !options.some((o) => o.value === pending);
}
