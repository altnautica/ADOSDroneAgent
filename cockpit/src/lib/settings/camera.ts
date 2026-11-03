// The Camera settings page model: the legal clockwise rotations the camera
// config key accepts, and the flip keys.

export const ROTATIONS = ["0", "90", "180", "270"] as const;
export type Rotation = (typeof ROTATIONS)[number];

export const ROTATION_HINT = {
  "0": "settings.camera.rotation0",
  "90": "settings.camera.rotation90",
  "180": "settings.camera.rotation180",
  "270": "settings.camera.rotation270",
} as const satisfies Record<Rotation, string>;

export const CAMERA_KEYS = {
  rotation: "video.camera.rotation",
  hflip: "video.camera.hflip",
  vflip: "video.camera.vflip",
} as const;

/** The stored rotation as one of the legal options, or null when the stored
 *  value is absent or not one of them (shown as unknown, never guessed). */
export function toRotation(v: unknown): Rotation | null {
  const s = typeof v === "number" || typeof v === "string" ? String(v) : null;
  return s !== null && (ROTATIONS as readonly string[]).includes(s) ? (s as Rotation) : null;
}

/** The rotation label: degrees. */
export function rotationLabel(r: string): string {
  return `${r}°`;
}
