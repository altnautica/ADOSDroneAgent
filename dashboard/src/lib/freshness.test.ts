import { describe, expect, it } from "vitest";

import { hdopOrNull } from "@/hooks/use-vehicle-telemetry";
import { isLive, liveData } from "@/lib/freshness";

describe("live values never outlive their source", () => {
  it("shows a value only while the read is not in error and at most two intervals old", () => {
    const ok = { isError: false, dataUpdatedAt: 10_000, data: 1 };
    expect(liveData(ok, 1_000, 11_500)).toBe(1);
    expect(liveData(ok, 1_000, 12_001)).toBeUndefined();
    expect(liveData({ ...ok, isError: true }, 1_000, 10_100)).toBeUndefined();
    expect(isLive({ isError: false, dataUpdatedAt: 0 }, 1_000, 10)).toBe(false);
  });
});

describe("HDOP sentinel", () => {
  it("drops the 655.35 unknown and non-positive values as the snapshot path does", () => {
    expect(hdopOrNull(655.35)).toBeNull();
    expect(hdopOrNull(0)).toBeNull();
    expect(hdopOrNull(null)).toBeNull();
    expect(hdopOrNull(0.7)).toBe(0.7);
  });
});
