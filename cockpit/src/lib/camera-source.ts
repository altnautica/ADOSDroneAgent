// The Settings leaf for `video.camera.source`: a picker over the cameras the
// agent detected (`GET /api/video/cameras`), led by the stock `csi` / `usb`
// hints (the agent picks the first camera of that kind), with free text kept
// as "Other…" for a device the scan does not list.

import type { VideoCamera } from "@/lib/video-model";
import i18n from "@/shared/i18n";

export const CAMERA_SOURCE_PATH = "video.camera.source";

/** The picker's sentinel for "type a device path". Not a legal path. */
export const OTHER_SOURCE = "\u0000other";

export interface CameraSourceOption {
  value: string;
  label: string;
}

/** The stock device hints `video.camera.source` takes besides a path. */
export const CAMERA_SOURCE_HINTS = ["csi", "usb"] as const;
const HINT_KEYS = {
  csi: "cockpit.settingsSections.camera.hintCsi",
  usb: "cockpit.settingsSections.camera.hintUsb",
} as const;

/** The stock hints, then one option per detected camera with a device path,
 *  labelled with its name and path, de-duplicated by path, in the order the
 *  agent listed them. */
export function cameraSourceOptions(cameras: readonly VideoCamera[]): CameraSourceOption[] {
  const seen = new Set<string>();
  const out: CameraSourceOption[] = CAMERA_SOURCE_HINTS.map((h) => ({ value: h, label: i18n.t(HINT_KEYS[h]) }));
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
