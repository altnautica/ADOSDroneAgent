// The Capture screen's controls over the world-engine extension's capture
// session. Stop closes the session (no resume), so it is fired from a guarded
// button; REC and MARK are plain. The extension answers 404 when absent.

import { apiFetch } from "@/shared/api-fetch";
import { CAPTURE_STATE_PATH, type CaptureState } from "@/shared/api/capture";

export const KNOWN_CAPTURE_ERRORS = ["E_STORAGE_FULL", "E_ALREADY_RECORDING", "E_CLOCK_NOT_READY"] as const;
export type KnownCaptureError = (typeof KNOWN_CAPTURE_ERRORS)[number];

export function getCaptureState(signal?: AbortSignal): Promise<CaptureState> {
  return apiFetch<CaptureState>(CAPTURE_STATE_PATH, { signal });
}

export interface CaptureControls {
  recording: boolean;
  floor: number | null;
  canStart: boolean;
  canStop: boolean;
  canMark: boolean;
}

/** Which controls are live for the session state and the operator's room and
 *  floor entries. A floor must be a whole number; MARK also needs a room. */
export function captureControls(state: CaptureState, room: string, floor: string, busy: boolean): CaptureControls {
  const recording = state.state === "recording" || state.state === "paused";
  const trimmed = floor.trim();
  const parsed = /^-?\d+$/.test(trimmed) ? Number(trimmed) : null;
  return {
    recording,
    floor: parsed,
    canStart: !busy && !recording && state.state !== "closing" && parsed != null,
    canStop: !busy && recording,
    canMark: !busy && recording && parsed != null && room.trim().length > 0,
  };
}

/** The start body: the room only when one was entered. */
export function startBody(room: string, floor: number): { room?: string; floor: number } {
  const r = room.trim();
  return r ? { room: r, floor } : { floor };
}

/** The locale key for a capture refusal code. */
export function captureErrorKey(code: string): KnownCaptureError | "other" {
  return (KNOWN_CAPTURE_ERRORS as readonly string[]).includes(code) ? (code as KnownCaptureError) : "other";
}
