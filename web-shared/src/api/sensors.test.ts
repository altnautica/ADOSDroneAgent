import { describe, expect, it } from "vitest";

import { formatSigma } from "./sensors";

describe("formatSigma", () => {
  it("prints nanoseconds below one microsecond", () => {
    expect(formatSigma(420)).toBe("420 ns");
    expect(formatSigma(0)).toBe("0 ns");
  });

  it("prints microseconds and milliseconds above that", () => {
    expect(formatSigma(1500)).toBe("2 µs");
    expect(formatSigma(2_500_000)).toBe("2.50 ms");
  });

  it("prints a dash for an unknown value", () => {
    expect(formatSigma(-1)).toBe("—");
  });
});
