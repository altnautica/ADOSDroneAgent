// `?demo=1` tells one story at a time: while the mock follow-me cycle runs,
// the mock pilot reports the follow-me skill as owner; while the mock mission
// flies, follow-me is idle.

import { afterEach, describe, expect, it, vi } from "vitest";

import { layerFrames } from "@/lib/layer-frames";
import { mockLayerStream } from "@/lib/mock-layers";
import { mockNavStream } from "@/lib/mock-nav";
import { useNavStreamStore } from "@/stores/nav-stream-store";

afterEach(() => {
  mockNavStream.stop();
  mockLayerStream.stop();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("demo feed", () => {
  it("never shows follow-me running under a mission owner", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    vi.stubGlobal("window", globalThis);
    mockNavStream.start();
    mockLayerStream.start();
    const seen = new Set<string>();
    // A minute covers a mission leg and a whole follow-me cycle.
    for (let at = 0; at < 60_000; at += 500) {
      await vi.advanceTimersByTimeAsync(500);
      const { follow, pilot } = useNavStreamStore.getState();
      expect(pilot).not.toBeNull();
      expect(follow).not.toBeNull();
      const owner = pilot!.data.owner;
      seen.add(owner);
      if (follow!.data.active) {
        expect(owner).toBe("skill");
        expect(pilot!.data.skill?.label).toBe("Follow me");
        expect(pilot!.data.mission).toBeNull();
      } else if (owner === "mission_fc") {
        expect(follow!.data.phase).toBe("idle");
      }
    }
    expect(seen).toEqual(new Set(["mission_fc", "skill"]));
  });

  it("pins pilot fields over the scene and stalls the layers under a live pilot", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    vi.stubGlobal("window", globalThis);
    mockLayerStream.start();
    await vi.advanceTimersByTimeAsync(100);
    const demo = window.__adosDemoPilot!;
    demo.pin({ input_session: { src: "bench", label: "Bench", session: 7 } });
    demo.stallLayers(true);
    await vi.advanceTimersByTimeAsync(100);
    const pilotAt = useNavStreamStore.getState().pilot!.at;
    const obstaclesSeen = layerFrames.obstacles.seen;
    await vi.advanceTimersByTimeAsync(500);
    const pilot = useNavStreamStore.getState().pilot!;
    expect(pilot.data.inputSession?.label).toBe("Bench");
    expect(pilot.at).toBeGreaterThan(pilotAt);
    expect(layerFrames.obstacles.seen).toBe(obstaclesSeen);
    // Stopping the feed drops the pins and the hook.
    mockLayerStream.stop();
    expect(window.__adosDemoPilot).toBeUndefined();
    mockLayerStream.start();
    await vi.advanceTimersByTimeAsync(100);
    expect(useNavStreamStore.getState().pilot!.data.inputSession ?? null).toBeNull();
    expect(layerFrames.obstacles.seen).not.toBe(obstaclesSeen);
  });
});
