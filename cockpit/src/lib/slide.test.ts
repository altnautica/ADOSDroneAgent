import { describe, expect, it } from "vitest";

import { slideCompletes } from "@/lib/slide";

describe("slideCompletes", () => {
  it("completes only for a drag that started on the thumb and crossed the track", () => {
    expect(slideCompletes(0.05, 0.95)).toBe(true);
    expect(slideCompletes(0.05, 0.6)).toBe(false);
  });

  it("never completes for a press that did not start on the thumb", () => {
    expect(slideCompletes(null, 0.99)).toBe(false);
    expect(slideCompletes(0.95, 0.95)).toBe(false);
    expect(slideCompletes(0.5, 1)).toBe(false);
  });
});
