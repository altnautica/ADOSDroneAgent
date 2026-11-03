// The Capture settings page's logic: the capture storage tone and the exposure
// draft sent to the world-engine extension's capture service. The draft keeps
// the typed numbers as text, so a half-typed "1." stays on screen; it becomes a
// request only when every field is valid.

import type { Tone } from "@/lib/tone";
import type { CaptureExposure } from "@/shared/api/capture";
import type { StorageStatus } from "@/shared/api/sensors";

export const STORAGE_POLL_MS = 10_000;
/** The capture state read. It feeds only the exposure form, which re-seeds on
 *  a changed value and never mid-edit, so a slow cadence is enough. */
export const CAPTURE_STATE_POLL_MS = 10_000;

export type ExposureMode = NonNullable<CaptureExposure["mode"]>;
export const EXPOSURE_MODES: readonly ExposureMode[] = ["walking", "slow", "short"];

/** The capture budget state's tone. */
export function captureStateTone(state: NonNullable<StorageStatus["capture"]>["state"]): Tone {
  if (state === "ok") return "ok";
  return state === "full" ? "err" : "warn";
}

export interface ExposureDraft {
  locked: boolean;
  exposureUs: string;
  gain: string;
  mode: ExposureMode | null;
}

/** The draft for the agent's reported exposure (null values read blank). */
export function toDraft(e: CaptureExposure): ExposureDraft {
  return {
    locked: e.locked,
    exposureUs: e.exposure_us == null ? "" : String(e.exposure_us),
    gain: e.gain == null ? "" : String(e.gain),
    mode: e.mode,
  };
}

function positive(text: string): number | null {
  const trimmed = text.trim();
  if (trimmed === "") return null;
  const n = Number(trimmed);
  return Number.isFinite(n) && n > 0 ? n : null;
}

/** The exposure request for a valid draft (positive exposure and gain, a
 *  mode), else null; Apply stays disabled while it is null. */
export function exposureRequest(draft: ExposureDraft): CaptureExposure | null {
  const exposure = positive(draft.exposureUs);
  const gain = positive(draft.gain);
  if (exposure == null || gain == null || draft.mode == null) return null;
  return { locked: draft.locked, exposure_us: exposure, gain, mode: draft.mode };
}

/** The fields the operator edits, as one comparable value. */
export function exposureSeedKey(e: CaptureExposure | null): string {
  return e ? `${e.locked}|${e.exposure_us}|${e.gain}|${e.mode}` : "";
}

/** True when the draft differs from what the agent reports. */
export function exposureDirty(draft: ExposureDraft, current: CaptureExposure): boolean {
  const base = toDraft(current);
  return (
    draft.locked !== base.locked ||
    draft.exposureUs.trim() !== base.exposureUs ||
    draft.gain.trim() !== base.gain ||
    draft.mode !== base.mode
  );
}

/** A lock was requested but the camera has not taken it. */
export function exposureNotApplied(current: CaptureExposure | null): boolean {
  return current?.locked === true && current.applied === false;
}
