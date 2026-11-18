import { describe, expect, it } from "vitest";

import {
  CLEAN_WINDOWS_TO_RELAX,
  RELAX_DWELL_MS,
  initialJitterState,
  jitterSampleBetween,
  jitterTargetForRung,
  jitterTunedReader,
  nextJitterTarget,
  type JitterControllerState,
  type JitterSample,
} from "./jitter";
import type { InboundVideoCounters } from "./video-stats";

const clean = (nowMs: number): JitterSample => ({ freezeDelta: 0, rtpJitterMs: 2, lossFraction: 0, nowMs });

const counters = (over: Partial<InboundVideoCounters> = {}): InboundVideoCounters => ({
  framesReceived: 0,
  framesDecoded: 0,
  framesDropped: 0,
  totalInterFrameDelay: 0,
  totalSquaredInterFrameDelay: 0,
  jitterBufferDelay: 0,
  jitterBufferEmittedCount: 0,
  bytesReceived: 0,
  packetsReceived: 0,
  packetsLost: 0,
  jitter: 0,
  freezeCount: 0,
  decoderImplementation: null,
  ...over,
});

describe("nextJitterTarget", () => {
  it("starts with no floor and steps up to 100 ms on a freeze", () => {
    expect(jitterTargetForRung(initialJitterState().rung)).toBe(0);
    const d = nextJitterTarget(initialJitterState(), { ...clean(1000), freezeDelta: 1 });
    expect(d).toMatchObject({ targetMs: 100, changed: true, reason: "freeze" });
  });

  it("keeps no floor on a clean or mildly jittery link", () => {
    let state: JitterControllerState = initialJitterState();
    for (let t = 1000; t <= 40_000; t += 1000) {
      const d = nextJitterTarget(state, { ...clean(t), rtpJitterMs: t % 2000 ? 2 : 20 });
      expect(d.changed).toBe(false);
      state = d;
    }
    expect(state.rung).toBe(0);
  });

  it("holds a step for the relax dwell, then relaxes back to no floor", () => {
    let state: JitterControllerState = nextJitterTarget(initialJitterState(), { ...clean(0), freezeDelta: 1 });
    expect(state.rung).toBe(1);
    let t = 0;
    // Ten clean windows pass before 15 s: the step holds.
    for (let i = 0; i < CLEAN_WINDOWS_TO_RELAX + 2; i += 1) {
      t += 1000;
      state = nextJitterTarget(state, clean(t));
    }
    expect(t).toBeLessThan(RELAX_DWELL_MS);
    expect(state.rung).toBe(1);
    let relaxed = null;
    while (t < RELAX_DWELL_MS) {
      t += 1000;
      const d = nextJitterTarget(state, clean(t));
      if (d.changed) relaxed = d;
      state = d;
    }
    expect(relaxed).toMatchObject({ targetMs: 0, reason: "sustained-clean" });
  });
});

describe("jitterSampleBetween", () => {
  it("reads loss, jitter and freezes from the deltas", () => {
    const s = jitterSampleBetween(
      counters({ packetsReceived: 100, packetsLost: 1, freezeCount: 2 }),
      counters({ packetsReceived: 196, packetsLost: 5, freezeCount: 3, jitter: 0.012 }),
      5000,
    );
    expect(s.freezeDelta).toBe(1);
    expect(s.rtpJitterMs).toBeCloseTo(12);
    expect(s.lossFraction).toBeCloseTo(4 / 100);
    expect(s.nowMs).toBe(5000);
  });

  it("clamps counters that went backwards (a new receiver) to no change", () => {
    const s = jitterSampleBetween(
      counters({ packetsReceived: 500, packetsLost: 9, freezeCount: 4 }),
      counters({ packetsReceived: 10, packetsLost: 0, freezeCount: 0 }),
      0,
    );
    expect(s.freezeDelta).toBe(0);
    expect(s.lossFraction).toBe(0);
  });
});

describe("jitterTunedReader", () => {
  it("retunes the video receiver only when the decision changes the depth", async () => {
    const readings = [
      counters({ packetsReceived: 100 }),
      counters({ packetsReceived: 200 }),
      counters({ packetsReceived: 300, freezeCount: 1 }),
      counters({ packetsReceived: 400, freezeCount: 1 }),
    ];
    let i = 0;
    const receiver = { track: { kind: "video" }, jitterBufferTarget: null as number | null };
    const writes: number[] = [];
    const tunable = new Proxy(receiver, {
      set(target, key, value) {
        if (key === "jitterBufferTarget") writes.push(value as number);
        return Reflect.set(target, key, value);
      },
    });
    const pc = {
      getStats: async () => new Map([["v", { type: "inbound-rtp", kind: "video", ...readings[i++] }]]) as unknown as RTCStatsReport,
      getReceivers: () => [tunable] as unknown as RTCRtpReceiver[],
    };
    let now = 0;
    const read = jitterTunedReader(pc, () => (now += 1000));

    expect(await read()).toMatchObject({ packetsReceived: 100 });
    await read(); // clean: no change
    expect(writes).toEqual([]);
    await read(); // a freeze: one step up from no floor
    expect(writes).toEqual([100]);
    await read(); // clean again, inside the dwell: no change
    expect(writes).toEqual([100]);
    expect(receiver.jitterBufferTarget).toBe(100);
  });
});
