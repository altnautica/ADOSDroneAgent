import { describe, expect, it } from "vitest";

import { isValidPin } from "./pin";

describe("isValidPin", () => {
  it("accepts the 4-12 digit PINs the agent accepts", () => {
    // A 6-digit PIN set from the cockpit or Mission Control must be enterable.
    expect(isValidPin("1234")).toBe(true);
    expect(isValidPin("123456")).toBe(true);
    expect(isValidPin("123456789012")).toBe(true);
  });

  it("rejects what the agent rejects", () => {
    expect(isValidPin("123")).toBe(false);
    expect(isValidPin("1234567890123")).toBe(false);
    expect(isValidPin("12a4")).toBe(false);
  });
});
