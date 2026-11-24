import { describe, expect, it } from "vitest";

import { boundPeerId, shortFingerprint } from "@/shared/api/pair-view";

describe("bound drone", () => {
  it("names the drone from the fleet slot when the config peer is absent", () => {
    expect(boundPeerId({ paired_with_device_id: null, slots: [{ slot: 1, device_id: "77735cd38937" }] })).toBe(
      "77735cd38937",
    );
    expect(
      boundPeerId({
        slots: [
          { slot: 0, device_id: "a" },
          { slot: 1, device_id: "b", video_hero: true },
        ],
      }),
    ).toBe("b");
    expect(boundPeerId({ paired_with_device_id: "cfg", slots: [{ slot: 0, device_id: "a" }] })).toBe("cfg");
    expect(boundPeerId(null)).toBeNull();
  });

  it("shortens the key fingerprint", () => {
    expect(shortFingerprint("68e0e30ced307f4e")).toBe("68e0…");
    expect(shortFingerprint(null)).toBeNull();
  });
});
