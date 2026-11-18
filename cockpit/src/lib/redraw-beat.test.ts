import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { BEAT_MS, beatNow, beatPhase, subscribeBeat } from "./redraw-beat";

describe("redraw beat", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("calls every subscriber once per beat on one shared tick", () => {
    const a = vi.fn();
    const b = vi.fn();
    const offA = subscribeBeat(a);
    const offB = subscribeBeat(b);
    vi.advanceTimersByTime(BEAT_MS * 3);
    expect(a).toHaveBeenCalledTimes(3);
    expect(b).toHaveBeenCalledTimes(3);
    expect(vi.getTimerCount()).toBe(1);
    offA();
    offB();
  });

  it("stops the interval after the last subscriber leaves", () => {
    const a = vi.fn();
    const off = subscribeBeat(a);
    vi.advanceTimersByTime(BEAT_MS);
    off();
    expect(vi.getTimerCount()).toBe(0);
    const before = beatNow();
    vi.advanceTimersByTime(500);
    expect(a).toHaveBeenCalledTimes(1);
    expect(beatNow()).toBe(before);
  });

  it("moves a five-beat phase once every 500 ms", () => {
    const off = subscribeBeat(() => {});
    // Align to a phase boundary, whatever earlier tests left the count at.
    while (beatNow() % 5 !== 0) vi.advanceTimersByTime(BEAT_MS);
    const start = beatPhase(5);
    vi.advanceTimersByTime(400);
    expect(beatPhase(5)).toBe(start);
    vi.advanceTimersByTime(100);
    expect(beatPhase(5)).toBe(start + 1);
    off();
  });
});
