import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DetectionDemandResponse } from "@/lib/api";
import {
  detectorNotice,
  LEASE_HOLD_S,
  LEASE_RENEW_MS,
  startDetectionLease,
  type DemandRequest,
  type DetectorState,
} from "@/lib/detection-lease";

function reply(detecting: boolean): DetectionDemandResponse {
  return { camera_id: "main", detecting, until_ms: detecting ? 6000 : null };
}

/** Let settled request promises deliver their callbacks. */
async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

describe("startDetectionLease", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("requests a 6 s hold now and renews every 2 s", async () => {
    const request = vi.fn<DemandRequest>(async () => reply(true));
    const stop = startDetectionLease("belly", request, () => {});
    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenLastCalledWith("belly", LEASE_HOLD_S);
    expect(LEASE_HOLD_S).toBe(6);
    expect(LEASE_RENEW_MS).toBe(2000);

    vi.advanceTimersByTime(LEASE_RENEW_MS);
    expect(request).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(LEASE_RENEW_MS * 2);
    expect(request).toHaveBeenCalledTimes(4);
    expect(request.mock.calls.every(([cam, hold]) => cam === "belly" && hold === 6)).toBe(true);
    stop();
    await flush();
  });

  it("releases with hold 0 on stop and stops renewing", async () => {
    const request = vi.fn(async () => reply(true));
    const stop = startDetectionLease("main", request, () => {});
    stop();
    expect(request).toHaveBeenLastCalledWith("main", 0);
    const calls = request.mock.calls.length;
    vi.advanceTimersByTime(LEASE_RENEW_MS * 5);
    expect(request).toHaveBeenCalledTimes(calls);
    stop();
    expect(request).toHaveBeenCalledTimes(calls);
    await flush();
  });

  it("reports starting, then the engine's answer", async () => {
    const states: DetectorState[] = [];
    let detecting = true;
    const request = vi.fn(async () => reply(detecting));
    const stop = startDetectionLease("main", request, (s) => states.push(s));
    expect(states).toEqual(["starting"]);
    await flush();
    expect(states).toEqual(["starting", "detecting"]);
    detecting = false;
    vi.advanceTimersByTime(LEASE_RENEW_MS);
    await flush();
    expect(states.at(-1)).toBe("idle");
    stop();
    await flush();
  });

  it("reports unavailable when the request fails, and nothing after stop", async () => {
    const states: DetectorState[] = [];
    const request = vi.fn(async (_cam: string, hold: number) => {
      if (hold > 0) throw new Error("503");
      return reply(false);
    });
    const stop = startDetectionLease("main", request, (s) => states.push(s));
    await flush();
    expect(states).toEqual(["starting", "unavailable"]);
    vi.advanceTimersByTime(LEASE_RENEW_MS);
    stop();
    await flush();
    expect(states).toEqual(["starting", "unavailable"]);
  });

  it("swallows a failed release", async () => {
    const request = vi.fn(async (_cam: string, hold: number) => {
      if (hold === 0) throw new Error("gone");
      return reply(true);
    });
    const stop = startDetectionLease("main", request, () => {});
    expect(() => stop()).not.toThrow();
    await flush();
  });
});

describe("detectorNotice", () => {
  it("speaks only when the detector is not running", () => {
    expect(detectorNotice("idle")).toBe("Detector idle");
    expect(detectorNotice("unavailable")).toBe("Detector unavailable");
    expect(detectorNotice("detecting")).toBeNull();
    expect(detectorNotice("starting")).toBeNull();
    expect(detectorNotice(null)).toBeNull();
  });
});
