import { describe, expect, it } from "vitest";

import { readInboundVideo, videoStatsBetween, type InboundVideoCounters } from "./video-stats";

const base: InboundVideoCounters = {
  framesReceived: 100,
  framesDecoded: 100,
  framesDropped: 1,
  totalInterFrameDelay: 3,
  totalSquaredInterFrameDelay: 0.1,
  jitterBufferDelay: 2,
  jitterBufferEmittedCount: 100,
  bytesReceived: 1_000_000,
  decoderImplementation: "ExternalDecoder",
};

describe("videoStatsBetween", () => {
  it("derives decoded, shown and received fps, drops, inter-frame spread, jitter buffer and rate from two samples", () => {
    // 30 frames decoded in 1 s with inter-frame delays averaging 33.3 ms and
    // spreading 10 ms: sum 1.0 s, sum of squares 30 * (mean^2 + sd^2).
    const mean = 1 / 30;
    const cur: InboundVideoCounters = {
      ...base,
      framesReceived: 131,
      framesDecoded: 130,
      framesDropped: 3,
      totalInterFrameDelay: 4,
      totalSquaredInterFrameDelay: 0.1 + 30 * (mean * mean + 0.01 * 0.01),
      jitterBufferDelay: 2.6,
      jitterBufferEmittedCount: 130,
      bytesReceived: 1_500_000,
    };
    const s = videoStatsBetween(
      { at: 10_000, presentedFrames: 50, inbound: base },
      { at: 11_000, presentedFrames: 70, inbound: cur },
    );
    expect(s.decodedFps).toBeCloseTo(30);
    expect(s.receivedFps).toBeCloseTo(31);
    expect(s.shownFps).toBeCloseTo(20);
    expect(s.droppedFrames).toBe(2);
    expect(s.interFrameMs).toBeCloseTo(33.33, 1);
    expect(s.interFrameSdMs).toBeCloseTo(10, 3);
    expect(s.jitterBufferMs).toBeCloseTo(20);
    expect(s.kbps).toBeCloseTo(4000);
    expect(s.decoder).toBe("ExternalDecoder");
  });

  it("scales by the real interval between samples", () => {
    const s = videoStatsBetween(
      { at: 0, presentedFrames: 0, inbound: base },
      { at: 2000, presentedFrames: 30, inbound: { ...base, framesDecoded: 160 } },
    );
    expect(s.decodedFps).toBeCloseTo(30);
    expect(s.shownFps).toBeCloseTo(15);
  });

  it("reports only the shown rate for a transport with no receiver counters", () => {
    const s = videoStatsBetween({ at: 0, presentedFrames: 10, inbound: null }, { at: 1000, presentedFrames: 35, inbound: null });
    expect(s.shownFps).toBeCloseTo(25);
    expect(s.decodedFps).toBeNull();
    expect(s.interFrameSdMs).toBeNull();
    expect(s.kbps).toBeNull();
  });

  it("drops a rate whose counter went backwards (a new receiver) rather than report a negative", () => {
    const s = videoStatsBetween(
      { at: 0, presentedFrames: 500, inbound: base },
      { at: 1000, presentedFrames: 4, inbound: { ...base, framesDecoded: 3, bytesReceived: 10 } },
    );
    expect(s.shownFps).toBeNull();
    expect(s.decodedFps).toBeNull();
    expect(s.kbps).toBeNull();
  });
});

describe("readInboundVideo", () => {
  it("reads the video inbound-rtp entry and ignores the rest", async () => {
    const report = new Map<string, Record<string, unknown>>([
      ["a", { type: "inbound-rtp", kind: "audio", bytesReceived: 9 }],
      ["v", { type: "inbound-rtp", kind: "video", ...base, framesDropped: undefined }],
      ["r", { type: "remote-inbound-rtp", kind: "video", bytesReceived: 7 }],
    ]);
    const counters = await readInboundVideo({ getStats: async () => report as unknown as RTCStatsReport });
    expect(counters).toEqual({ ...base, framesDropped: 0 });
  });

  it("is null before the receiver exists", async () => {
    expect(await readInboundVideo({ getStats: async () => new Map() as unknown as RTCStatsReport })).toBeNull();
  });
});
