import { describe, expect, it } from "vitest";

import { captureControls, captureErrorKey, startBody } from "@/lib/capture-model";
import type { CaptureState } from "@/shared/api/capture";

const state = (s: CaptureState["state"]): CaptureState => ({
  state: s,
  session_id: s === "idle" ? null : "S1",
  duration_s: 10,
  site: null,
  floor: 0,
  room: null,
  segment: 1,
  frames: null,
  health: [],
  exposure: null,
  storage: null,
});

describe("capture controls", () => {
  it("starts from idle with a whole-number floor", () => {
    expect(captureControls(state("idle"), "", "2", false)).toMatchObject({ canStart: true, canStop: false, canMark: false, floor: 2 });
    expect(captureControls(state("idle"), "", "-1", false).canStart).toBe(true);
    expect(captureControls(state("idle"), "", "1.5", false).canStart).toBe(false);
    expect(captureControls(state("closing"), "", "1", false).canStart).toBe(false);
  });

  it("marks only while recording and with a room", () => {
    expect(captureControls(state("recording"), "", "1", false).canMark).toBe(false);
    expect(captureControls(state("recording"), "Lab", "1", false).canMark).toBe(true);
  });

  it("sends the room only when one was entered", () => {
    expect(startBody("  ", 3)).toEqual({ floor: 3 });
    expect(startBody(" Lab ", 3)).toEqual({ room: "Lab", floor: 3 });
  });

  it("maps refusal codes to their locale keys", () => {
    expect(captureErrorKey("E_STORAGE_FULL")).toBe("E_STORAGE_FULL");
    expect(captureErrorKey("E_WHATEVER")).toBe("other");
  });
});

describe("stop gate", () => {
  it("offers the guarded stop only for a running or paused session while idle", () => {
    expect(captureControls(state("recording"), "", "0", false).canStop).toBe(true);
    expect(captureControls(state("paused"), "", "0", false).canStop).toBe(true);
    expect(captureControls(state("recording"), "", "0", true).canStop).toBe(false);
    expect(captureControls(state("idle"), "", "0", false).canStop).toBe(false);
  });
});
