import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createVideoTransport,
  type DialResult,
  type TransportKind,
  type VideoTransportSnapshot,
  VIDEO_RECOVER_WINDOW_MS,
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

/** A scripted harness: each dial pops the next scripted outcome for its kind
 *  (or asks `answers`, when given), and frames are delivered by hand to
 *  whichever element is being watched. */
function harness(
  order: TransportKind[],
  script: Partial<Record<TransportKind, boolean[]>>,
  answers?: (kind: TransportKind) => boolean,
) {
  const video = fakeVideo();
  const probe = fakeVideo();
  const dials: { kind: TransportKind; el: HTMLVideoElement; at: number }[] = [];
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
      dials.push({ kind, el, at: Date.now() });
      lost.push(onLost);
      const ok = answers ? answers(kind) : (script[kind]?.shift() ?? true);
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

  it("backs off between full failures and resets once a frame is presented", async () => {
    const h = harness(["whep", "hls"], { whep: [false, false, false, false], hls: [false, false, false] });
    h.transport.start();
    await flush();
    const dialsAt = () => h.dials.length;
    expect(dialsAt()).toBe(2);
    await vi.advanceTimersByTimeAsync(3000);
    expect(dialsAt()).toBe(4);
    await vi.advanceTimersByTimeAsync(5999);
    expect(dialsAt()).toBe(4);
    await vi.advanceTimersByTimeAsync(1);
    expect(dialsAt()).toBe(6);
    await vi.advanceTimersByTimeAsync(12_000);
    expect(dialsAt()).toBe(8);
    h.frame();
    expect(h.snap().state).toBe("live");
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

describe("recovery once a live stream stops", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  /** The gaps between consecutive dials, in ms. */
  const gaps = (dials: { at: number }[]) => dials.slice(1).map((d, i) => d.at - dials[i].at);

  it("redials every second for a minute after the last frame, then backs off doubling", async () => {
    let refusing = false;
    const h = harness(["whep"], {}, () => !refusing);
    h.transport.start();
    await flush();
    await playFor(h, 5000);
    const lastFrameAt = Date.now() - 250;
    // The stream goes away: every dial is refused from now on.
    refusing = true;
    await vi.advanceTimersByTimeAsync(VIDEO_RECOVER_WINDOW_MS + 120_000);
    const after = h.dials.filter((d) => d.at > lastFrameAt);
    const fast = gaps(after.filter((d) => d.at - lastFrameAt <= VIDEO_RECOVER_WINDOW_MS));
    expect(fast.length).toBeGreaterThan(50);
    expect(new Set(fast)).toEqual(new Set([1000]));
    const slow = gaps(after.filter((d) => d.at - lastFrameAt >= VIDEO_RECOVER_WINDOW_MS));
    expect(slow.slice(0, 6)).toEqual([3000, 6000, 12_000, 24_000, 30_000, 30_000]);
  });

  it("never-live feeds keep the doubling backoff from the start", async () => {
    const h = harness(["whep"], {}, () => false);
    h.transport.start();
    await flush();
    await vi.advanceTimersByTimeAsync(80_000);
    expect(gaps(h.dials).slice(0, 4)).toEqual([3000, 6000, 12_000, 24_000]);
  });

  it("shows frames within 1.5 s of the path returning after an 11 s source outage", async () => {
    // The measured outage: the path stays ready but silent for the outage
    // (a dial answers, no frame comes), the agent then restarts its pipeline
    // (0.69 s after the source returns, dials refused) and the path serves
    // frames again 3.18 s after the source returned.
    const OUTAGE_MS = 11_000;
    const RESTART_AT = OUTAGE_MS + 690;
    const READY_AT = OUTAGE_MS + 3180;
    let t0: number | null = null;
    const phase = () => {
      if (t0 === null) return "ready";
      const t = Date.now() - t0;
      return t < RESTART_AT ? "silent" : t < READY_AT ? "absent" : "ready";
    };
    const h = harness(["whep", "hls"], {}, () => phase() !== "absent");
    h.transport.start();
    await flush();
    await playFor(h, 5000);
    t0 = Date.now();
    let backAt: number | null = null;
    for (let t = 0; t < 40_000 && backAt === null; t += 100) {
      const last = h.dials[h.dials.length - 1];
      // A session dialled into a ready path presents its first frame 400 ms on.
      if (phase() === "ready" && last.at - t0 >= READY_AT && Date.now() - last.at >= 400) {
        h.frame();
        if (h.snap().state === "live") backAt = Date.now() - t0;
      }
      await vi.advanceTimersByTimeAsync(100);
    }
    expect(backAt).not.toBeNull();
    expect(backAt! - READY_AT).toBeLessThanOrEqual(1500);
  });
});
