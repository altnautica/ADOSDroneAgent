import { describe, expect, it, vi } from "vitest";

import { droneStatusFromFull, visionEngineRunning } from "@/lib/api";
import type { StatusFull } from "@/shared/agent-api.generated";

import { hasLocalVision } from "./use-vision-detections";

/** A drone's `/api/status/full` as a generic x86 board answers it: the board
 *  profile declares no vision path (`perceptionTier: "none"`), while the
 *  operator-configured ONNX engine runs and detects. */
function genericBoardStatus(visionState: { state: string; sub_state: string } | null): StatusFull {
  const services = [
    { name: "ados-control", state: "active", sub_state: "running" },
    ...(visionState ? [{ name: "ados-vision", ...visionState }] : []),
  ];
  return { perceptionTier: "none", services, fc_connected: true } as unknown as StatusFull;
}

function gate(full: StatusFull, profile = "drone"): boolean {
  const s = droneStatusFromFull(full, null);
  return hasLocalVision({
    ownsCameras: profile === "drone" || profile === "visor",
    perceptionTier: s.perception_tier ?? null,
    visionEngine: s.vision_engine ?? null,
    profile,
  });
}

describe("the Feed's local vision gate", () => {
  it("opens on a drone whose engine runs although its board declares no vision path", () => {
    expect(gate(genericBoardStatus({ state: "active", sub_state: "running" }))).toBe(true);
  });

  it("stays shut on a drone with no engine running", () => {
    expect(gate(genericBoardStatus({ state: "inactive", sub_state: "dead" }))).toBe(false);
    expect(gate(genericBoardStatus({ state: "activating", sub_state: "auto-restart" }))).toBe(false);
    expect(gate(genericBoardStatus(null))).toBe(false);
  });

  it("follows a declared vision path, and never opens on a ground station", () => {
    const declared = { ...genericBoardStatus(null), perceptionTier: "local" } as StatusFull;
    expect(gate(declared)).toBe(true);
    expect(gate(genericBoardStatus({ state: "active", sub_state: "running" }), "ground_station")).toBe(false);
    expect(hasLocalVision({ ownsCameras: false, perceptionTier: null, visionEngine: null, profile: "workstation" })).toBe(true);
  });

  it("reads the engine only from a services list that names it", () => {
    expect(visionEngineRunning(undefined)).toBeNull();
    expect(visionEngineRunning([{ name: "ados-vision-rknn", state: "active", sub_state: "running" }])).toBeNull();
  });

  it("a demo camera node runs the synthetic detector with no status to read", () => {
    vi.stubGlobal("window", { location: { search: "?demo=1" } });
    try {
      expect(hasLocalVision({ ownsCameras: true, perceptionTier: null, visionEngine: null, profile: "drone" })).toBe(true);
      expect(hasLocalVision({ ownsCameras: false, perceptionTier: null, visionEngine: null, profile: "ground_station" })).toBe(false);
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
