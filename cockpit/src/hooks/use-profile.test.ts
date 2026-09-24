import { afterEach, describe, expect, it, vi } from "vitest";

import { PROFILE_RETRY_MS, probeProfile } from "./use-profile";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("probeProfile", () => {
  it("keeps asking until the agent answers, then resolves its profile", async () => {
    // A kiosk loads before the agent API is up: the first probe fails. Giving
    // up there left a drone treated as a ground station until a reload.
    vi.useFakeTimers();
    const fetchMock = vi
      .fn()
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ profile: "drone", pairing_code: null }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const settled = vi.fn();
    void probeProfile().then(settled);
    await vi.advanceTimersByTimeAsync(PROFILE_RETRY_MS + 10);

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(settled).toHaveBeenCalledWith("drone");
  });
});
