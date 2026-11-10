import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { create } from "zustand";

import { MAV_CAMERA_REFRESH_MS, reprobeOnNoVideo, watchMavCameras } from "./mav-camera-refresh";

function fakeStatus(fc: boolean) {
  return create<{ status: { fc_connected?: boolean } | null }>(() => ({ status: { fc_connected: fc } }));
}

describe("watchMavCameras", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("lists the cameras on mount, on the FC-connected edge and every 30 s", () => {
    const refresh = vi.fn();
    const status = fakeStatus(false);
    const stop = watchMavCameras(refresh, status);
    expect(refresh).toHaveBeenCalledTimes(1);
    status.setState({ status: { fc_connected: true } });
    expect(refresh).toHaveBeenCalledTimes(2);
    status.setState({ status: { fc_connected: true } });
    expect(refresh).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(MAV_CAMERA_REFRESH_MS);
    expect(refresh).toHaveBeenCalledTimes(3);
    stop();
    vi.advanceTimersByTime(MAV_CAMERA_REFRESH_MS);
    status.setState({ status: { fc_connected: false } });
    status.setState({ status: { fc_connected: true } });
    expect(refresh).toHaveBeenCalledTimes(3);
  });
});

describe("reprobeOnNoVideo", () => {
  it("re-lists the cameras only when the reason is no video", () => {
    const refresh = vi.fn();
    expect(reprobeOnNoVideo("No video", "No video", refresh)).toBe(true);
    expect(reprobeOnNoVideo("Not ready", "No video", refresh)).toBe(false);
    expect(reprobeOnNoVideo(null, "No video", refresh)).toBe(false);
    expect(refresh).toHaveBeenCalledTimes(1);
  });
});
