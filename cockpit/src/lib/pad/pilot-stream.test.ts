import { describe, expect, it } from "vitest";

import { PilotStreamer, stopReason, type StreamTick } from "@/lib/pad/pilot-stream";

function tick(over: Partial<StreamTick> = {}): StreamTick {
  return {
    now: 1000,
    enabled: true,
    connected: true,
    hidden: false,
    target: true,
    sampleAt: 1000,
    axes: [0, 0, 0, 0],
    buttons: 0,
    ...over,
  };
}

describe("stick stream stop rules", () => {
  it("keeps streaming a pad held perfectly still for seconds", () => {
    const sent: number[][] = [];
    let released = 0;
    const s = new PilotStreamer({ send: (a) => (sent.push([...a]), true), release: () => released++ });
    // The pad's own timestamp would be frozen; the poll clock keeps ticking.
    for (let t = 0; t < 3000; t += 20) {
      expect(s.tick(tick({ now: t, sampleAt: t, axes: [0, 1, 0, 0] }))).toBeNull();
    }
    expect(released).toBe(0);
    expect(sent.length).toBe(150);
    expect(sent.every((a) => a[1] === 1)).toBe(true);
  });

  it("stops once the poller has not read the pad for more than 150 ms", () => {
    expect(stopReason(tick({ now: 1151, sampleAt: 1000 }))).toBe("stale");
    expect(stopReason(tick({ now: 1150, sampleAt: 1000 }))).toBeNull();
    expect(stopReason(tick({ sampleAt: null }))).toBe("stale");
  });

  it("releases once when another screen or the quick menu takes the pad", () => {
    let released = 0;
    const s = new PilotStreamer({ send: () => true, release: () => released++ });
    s.tick(tick());
    expect(s.tick(tick({ target: false }))).toBe("away");
    expect(s.tick(tick({ target: false }))).toBe("away");
    expect(released).toBe(1);
  });

  it("orders the reasons: disabled, disconnected, hidden, away", () => {
    expect(stopReason(tick({ enabled: false, connected: false }))).toBe("disabled");
    expect(stopReason(tick({ connected: false, hidden: true }))).toBe("disconnected");
    expect(stopReason(tick({ hidden: true, target: false }))).toBe("hidden");
  });
});
