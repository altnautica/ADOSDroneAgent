import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { GIMBAL_RESEND_MS, createGimbalRateSender } from "@/lib/gimbal-rate";

describe("gimbal rate pacing", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("coalesces a drag to about one request per 100 ms, carrying the newest rate", () => {
    const sent: [number, number][] = [];
    const s = createGimbalRateSender((p, y) => sent.push([p, y]));
    // A 120 Hz drag for one second: a new rate every ~8 ms.
    for (let i = 1; i <= 120; i += 1) {
      s.set(i / 4, -i / 8);
      vi.advanceTimersByTime(8);
    }
    expect(sent.length).toBeLessThanOrEqual(11);
    expect(sent.length).toBeGreaterThanOrEqual(9);
    vi.advanceTimersByTime(100);
    expect(sent[sent.length - 1]).toEqual([30, -15]);
    // Within any 200 ms of the drag, at most 3 requests (the first goes at once).
    expect(sent.length).toBeLessThanOrEqual(12);
    s.dispose();
  });

  it("resends a held rate before the agent's hold runs out, and stops with one zero at once", () => {
    const sent: [number, number][] = [];
    const s = createGimbalRateSender((p, y) => sent.push([p, y]));
    s.set(20, 0);
    expect(sent).toEqual([[20, 0]]);
    vi.advanceTimersByTime(1000);
    // Held for 1 s: resent every GIMBAL_RESEND_MS (rounded up to the 100 ms tick).
    expect(sent.length).toBe(1 + Math.floor(1000 / (Math.ceil(GIMBAL_RESEND_MS / 100) * 100)));
    const before = sent.length;
    s.set(0, 0);
    expect(sent.length).toBe(before + 1);
    expect(sent[sent.length - 1]).toEqual([0, 0]);
    vi.advanceTimersByTime(1000);
    expect(sent.length).toBe(before + 1);
  });

  it("sends nothing while idle, and a zero on dispose only after a held rate", () => {
    const sent: [number, number][] = [];
    const s = createGimbalRateSender((p, y) => sent.push([p, y]));
    s.set(0, 0);
    s.dispose();
    expect(sent).toEqual([]);
    const t = createGimbalRateSender((p, y) => sent.push([p, y]));
    t.set(5, 5);
    t.dispose();
    expect(sent).toEqual([
      [5, 5],
      [0, 0],
    ]);
  });
});
