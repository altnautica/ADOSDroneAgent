import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createVideoTransport,
  type DialResult,
  type TransportKind,
  type VideoTransportSnapshot,
} from "./video-transport";

interface FakeVideo {
  videoWidth: number;
  videoHeight: number;
  srcObject: unknown;
}

function fakeVideo(): HTMLVideoElement {
  const v: FakeVideo = { videoWidth: 1280, videoHeight: 720, srcObject: null };
  return v as unknown as HTMLVideoElement;
}

/** A scripted harness: each dial pops the next scripted outcome for its kind,
 *  and frames are delivered by hand to whichever element is being watched. */
function harness(order: TransportKind[], script: Partial<Record<TransportKind, boolean[]>>) {
  const video = fakeVideo();
  const probe = fakeVideo();
  const dials: { kind: TransportKind; el: HTMLVideoElement }[] = [];
  const lost: (() => void)[] = [];
  const closed: TransportKind[] = [];
  const watchers = new Map<HTMLVideoElement, () => void>();
  const snaps: VideoTransportSnapshot[] = [];

  const transport = createVideoTransport({
    order,
    whepUrl: "/whep",
    hlsUrl: "/hls/main/index.m3u8",
    video,
    createProbe: () => probe,
    onChange: (s) => snaps.push(s),
    watchFrames: (el, onFrame) => {
      watchers.set(el, onFrame);
      return () => {
        if (watchers.get(el) === onFrame) watchers.delete(el);
      };
    },
    dial: async (kind, _url, el, onLost): Promise<DialResult> => {
      dials.push({ kind, el });
      lost.push(onLost);
      const ok = script[kind]?.shift() ?? true;
      if (!ok) return { ok: false, error: `${kind} refused` };
      return { ok: true, session: { close: () => void closed.push(kind) } };
    },
  });

  return {
    transport,
    video,
    probe,
    dials,
    lost,
    closed,
    snap: () => transport.snapshot(),
    frame: (el: HTMLVideoElement = video) => watchers.get(el)?.(),
  };
}

const flush = () => vi.advanceTimersByTimeAsync(0);

/** Advance `ms` while the main element keeps presenting frames. */
async function playFor(h: ReturnType<typeof harness>, ms: number) {
  for (let t = 0; t < ms; t += 250) {
    h.frame();
    await vi.advanceTimersByTimeAsync(250);
  }
}

describe("createVideoTransport", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("is live only once a frame is presented", async () => {
    const h = harness(["whep", "hls"], {});
    h.transport.start();
    await flush();
    expect(h.snap()).toMatchObject({ state: "connecting", transport: "whep" });
    h.frame();
    expect(h.snap()).toMatchObject({ state: "live", transport: "whep", width: 1280, height: 720 });
  });

  it("falls back exactly one step when the first transport fails", async () => {
    const h = harness(["whep", "hls"], { whep: [false] });
    h.transport.start();
    await flush();
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "hls"]);
    h.frame();
    expect(h.snap()).toMatchObject({ state: "live", transport: "hls", highLatency: true });
  });

  it("re-dials once on a freeze, then falls back on a second freeze within the window", async () => {
    const h = harness(["whep", "hls"], {});
    h.transport.start();
    await flush();
    h.frame();
    await vi.advanceTimersByTimeAsync(3000);
    expect(h.snap().state).toBe("frozen");
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "whep"]);
    expect(h.closed).toEqual(["whep"]);

    h.frame();
    expect(h.snap().state).toBe("live");
    await vi.advanceTimersByTimeAsync(3000);
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "whep", "hls"]);
    h.frame();
    expect(h.snap()).toMatchObject({ state: "live", transport: "hls", highLatency: true });
  });

  it("treats freezes further apart than the window as independent", async () => {
    const h = harness(["whep", "hls"], {});
    h.transport.start();
    await flush();
    h.frame();
    await vi.advanceTimersByTimeAsync(3000);
    // Keep frames flowing for longer than the freeze window.
    for (let t = 0; t < 65_000; t += 500) {
      h.frame();
      await vi.advanceTimersByTimeAsync(500);
    }
    await vi.advanceTimersByTimeAsync(3000);
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "whep", "whep"]);
  });

  it("moves down the order when an established session is lost", async () => {
    const h = harness(["whep", "hls"], {});
    h.transport.start();
    await flush();
    h.frame();
    h.lost[0]();
    await flush();
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "hls"]);
  });

  it("fails after the last transport and restarts from the top", async () => {
    const h = harness(["whep", "hls"], { whep: [false], hls: [false] });
    h.transport.start();
    await flush();
    expect(h.snap()).toMatchObject({ state: "failed", error: "hls refused" });
    await vi.advanceTimersByTimeAsync(3000);
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "hls", "whep"]);
  });

  it("counts a session that never presents a frame as a failure", async () => {
    const h = harness(["whep", "hls"], {});
    h.transport.start();
    await flush();
    await vi.advanceTimersByTimeAsync(8500);
    expect(h.dials.map((d) => d.kind)).toEqual(["whep", "hls"]);
  });

  it("retries WHEP in the background and adopts it only once it presents a frame", async () => {
    const h = harness(["whep", "hls"], { whep: [false] });
    h.transport.start();
    await flush();
    h.frame();
    expect(h.snap().transport).toBe("hls");

    await playFor(h, 60_000);
    const last = h.dials[h.dials.length - 1];
    expect(last).toMatchObject({ kind: "whep", el: h.probe });
    // Still on HLS until the probe shows a picture.
    expect(h.snap().transport).toBe("hls");

    h.frame(h.probe);
    await flush();
    expect(h.closed).toContain("hls");
    h.frame();
    expect(h.snap()).toMatchObject({ state: "live", transport: "whep", highLatency: false });
  });

  it("never retries WHEP when the order ranks it below the current transport", async () => {
    const h = harness(["hls", "whep"], {});
    h.transport.start();
    await flush();
    h.frame();
    await playFor(h, 120_000);
    expect(h.dials.map((d) => d.kind)).toEqual(["hls"]);
    expect(h.snap().highLatency).toBe(false);
  });

  it("closes the session on stop and ignores a late dial", async () => {
    const h = harness(["whep"], {});
    h.transport.start();
    await flush();
    h.frame();
    h.transport.stop();
    expect(h.closed).toEqual(["whep"]);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(h.dials).toHaveLength(1);
  });
});
