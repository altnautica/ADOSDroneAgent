// The one video policy both agent SPAs play through.
//
// A transport that negotiated is only a candidate: the feed is `live` once a
// frame has actually been presented, and it is `frozen` once frames stop for
// `noFrameMs`. Frame progress comes from `requestVideoFrameCallback` (with a
// `timeupdate` fallback where it is missing), never from the handshake.
//
// Policy, per failure exactly one step:
//   - dial `order[0]`; a dial failure, a lost session or no first frame moves
//     to the next transport; when the last one fails the state is `failed` and
//     the cascade restarts from the top after `retryMs`, doubling on each
//     consecutive full failure up to `maxRetryMs` (reset by a presented frame),
//     so a node with no video costs a few requests a minute, not a tight loop;
//   - a freeze re-dials the current transport once; a second freeze within
//     `freezeWindowMs` moves to the next transport (flagged `highLatency` when
//     that step trades WHEP for HLS);
//   - while playing on a transport ranked below WHEP, WHEP is re-tried every
//     `primaryRetryMs` on an off-screen element and adopted only once it
//     presents a frame, so a viewer whose WHEP never works keeps its picture.

import { startHls } from "./hls";
import { startWhep } from "./whep";

export type TransportKind = "whep" | "hls";
export type VideoFeedState = "connecting" | "live" | "frozen" | "failed";

export interface VideoTransportSnapshot {
  state: VideoFeedState;
  transport: TransportKind | null;
  /** True while playing HLS because WHEP failed or froze. */
  highLatency: boolean;
  error: string | null;
  width: number | null;
  height: number | null;
}

export interface TransportSession {
  close: () => void | Promise<void>;
}

export interface DialResult {
  ok: boolean;
  session?: TransportSession;
  error?: string;
}

export type TransportDialer = (
  kind: TransportKind,
  url: string,
  video: HTMLVideoElement,
  onLost: () => void,
) => Promise<DialResult>;

export type FrameWatcher = (video: HTMLVideoElement, onFrame: () => void) => () => void;

export interface VideoTransportOptions {
  order: TransportKind[];
  whepUrl: string;
  hlsUrl: string;
  video: HTMLVideoElement;
  noFrameMs?: number;
  firstFrameMs?: number;
  retryMs?: number;
  maxRetryMs?: number;
  freezeWindowMs?: number;
  primaryRetryMs?: number;
  onChange?: (snapshot: VideoTransportSnapshot) => void;
  dial?: TransportDialer;
  watchFrames?: FrameWatcher;
  /** An off-screen element for the background WHEP retry; null disables it. */
  createProbe?: () => HTMLVideoElement | null;
}

export interface VideoTransport {
  start: () => void;
  stop: () => void;
  snapshot: () => VideoTransportSnapshot;
}

export const VIDEO_NO_FRAME_MS = 2500;

/** The production dialer: WHEP sessions report a failed peer connection as
 *  lost; HLS sessions report hls.js giving up. */
export const defaultDialer: TransportDialer = async (kind, url, video, onLost) => {
  if (kind === "hls") return startHls(url, video, onLost);
  const res = await startWhep(url, video);
  const pc = res.session?.pc;
  pc?.addEventListener("connectionstatechange", () => {
    if (pc.connectionState === "failed") onLost();
  });
  return res;
};

type FrameCallbackVideo = HTMLVideoElement & {
  requestVideoFrameCallback?: (cb: () => void) => number;
  cancelVideoFrameCallback?: (handle: number) => void;
};

/** Frame progress from `requestVideoFrameCallback`, else `timeupdate`. */
export const defaultFrameWatcher: FrameWatcher = (video, onFrame) => {
  const v = video as FrameCallbackVideo;
  if (typeof v.requestVideoFrameCallback === "function") {
    let handle = 0;
    let stopped = false;
    const loop = () => {
      if (stopped) return;
      onFrame();
      handle = v.requestVideoFrameCallback!(loop);
    };
    handle = v.requestVideoFrameCallback(loop);
    return () => {
      stopped = true;
      v.cancelVideoFrameCallback?.(handle);
    };
  }
  let last = -1;
  const onTime = () => {
    if (video.currentTime !== last) {
      last = video.currentTime;
      onFrame();
    }
  };
  video.addEventListener("timeupdate", onTime);
  video.addEventListener("loadeddata", onTime);
  return () => {
    video.removeEventListener("timeupdate", onTime);
    video.removeEventListener("loadeddata", onTime);
  };
};

function defaultProbe(): HTMLVideoElement | null {
  if (typeof document === "undefined") return null;
  const el = document.createElement("video");
  el.muted = true;
  el.playsInline = true;
  el.autoplay = true;
  return el;
}

export function createVideoTransport(opts: VideoTransportOptions): VideoTransport {
  const {
    order,
    video,
    noFrameMs = VIDEO_NO_FRAME_MS,
    firstFrameMs = Math.max(8000, noFrameMs * 3),
    retryMs = 3000,
    maxRetryMs = 30_000,
    freezeWindowMs = 60_000,
    primaryRetryMs = 60_000,
    onChange,
    dial = defaultDialer,
    watchFrames = defaultFrameWatcher,
    createProbe = defaultProbe,
  } = opts;
  const urlFor = (kind: TransportKind) => (kind === "whep" ? opts.whepUrl : opts.hlsUrl);
  const whepRank = order.indexOf("whep");

  let snap: VideoTransportSnapshot = {
    state: "connecting",
    transport: null,
    highLatency: false,
    error: null,
    width: null,
    height: null,
  };
  let running = false;
  let gen = 0;
  let index = 0;
  let session: TransportSession | null = null;
  let stopFrames: (() => void) | null = null;
  let adoptedStream: MediaProvider | null = null;
  let lastFrameAt = 0;
  let sessionStartedAt = 0;
  let sawFrame = false;
  let freezes: number[] = [];
  let fullFailures = 0;
  let watchdog: ReturnType<typeof setInterval> | null = null;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let primaryTimer: ReturnType<typeof setTimeout> | null = null;
  let probeGen = 0;

  const emit = (patch: Partial<VideoTransportSnapshot>) => {
    const next = { ...snap, ...patch };
    const changed = (Object.keys(next) as (keyof VideoTransportSnapshot)[]).some(
      (k) => next[k] !== snap[k],
    );
    snap = next;
    if (changed) onChange?.(snap);
  };

  const clearTimers = () => {
    clearInterval(watchdog ?? undefined);
    clearTimeout(retryTimer ?? undefined);
    clearTimeout(primaryTimer ?? undefined);
    watchdog = retryTimer = primaryTimer = null;
    probeGen += 1;
  };

  const teardown = () => {
    clearTimers();
    stopFrames?.();
    stopFrames = null;
    const s = session;
    session = null;
    if (s) void Promise.resolve(s.close()).catch(() => undefined);
    if (adoptedStream && video.srcObject === adoptedStream) video.srcObject = null;
    adoptedStream = null;
  };

  const isHighLatency = (i: number) => order[i] === "hls" && whepRank !== -1 && whepRank < i;

  const onFrame = () => {
    lastFrameAt = Date.now();
    sawFrame = true;
    fullFailures = 0;
    const w = video.videoWidth > 0 ? video.videoWidth : null;
    const h = video.videoHeight > 0 ? video.videoHeight : null;
    if (snap.state !== "live" || snap.width !== w || snap.height !== h) {
      emit({ state: "live", error: null, width: w, height: h });
    }
  };

  const armSession = (myGen: number) => {
    sessionStartedAt = Date.now();
    lastFrameAt = 0;
    sawFrame = false;
    stopFrames = watchFrames(video, onFrame);
    const tick = Math.max(100, Math.min(500, Math.floor(noFrameMs / 2)));
    watchdog = setInterval(() => {
      if (myGen !== gen) return;
      const now = Date.now();
      if (!sawFrame) {
        if (now - sessionStartedAt >= firstFrameMs) failCurrent(myGen, "No video frames received.");
        return;
      }
      if (now - lastFrameAt >= noFrameMs) onFreeze(myGen);
    }, tick);
    schedulePrimaryRetry(myGen);
  };

  /** Dial `order[i]`. A freeze re-dial keeps showing `frozen` until a frame
   *  arrives, so the operator is not told the picture is merely connecting. */
  const dialIndex = async (i: number, state: VideoFeedState = "connecting") => {
    teardown();
    const myGen = ++gen;
    index = i;
    const kind = order[i];
    emit({ state, transport: kind, highLatency: isHighLatency(i) });
    const result = await dial(kind, urlFor(kind), video, () => {
      if (myGen === gen) failCurrent(myGen, "Video session lost.");
    });
    if (myGen !== gen || !running) {
      if (result.session) void Promise.resolve(result.session.close()).catch(() => undefined);
      return;
    }
    if (!result.ok || !result.session) {
      failCurrent(myGen, result.error ?? "Video transport failed.");
      return;
    }
    session = result.session;
    armSession(myGen);
  };

  /** One step down the order, or `failed` plus a restart from the top. */
  function failCurrent(myGen: number, error: string) {
    if (myGen !== gen || !running) return;
    if (index + 1 < order.length) {
      freezes = [];
      void dialIndex(index + 1);
      return;
    }
    teardown();
    gen += 1;
    emit({ state: "failed", error, width: null, height: null });
    const delay = Math.min(maxRetryMs, retryMs * 2 ** fullFailures);
    fullFailures += 1;
    retryTimer = setTimeout(() => {
      retryTimer = null;
      freezes = [];
      if (running) void dialIndex(0);
    }, delay);
  }

  function onFreeze(myGen: number) {
    if (myGen !== gen) return;
    const now = Date.now();
    freezes = freezes.filter((t) => now - t < freezeWindowMs);
    freezes.push(now);
    emit({ state: "frozen" });
    if (freezes.length >= 2 && index + 1 < order.length) {
      freezes = [];
      void dialIndex(index + 1);
    } else {
      void dialIndex(index, "frozen");
    }
  }

  function schedulePrimaryRetry(myGen: number) {
    if (whepRank === -1 || whepRank >= index) return;
    const myProbe = ++probeGen;
    primaryTimer = setTimeout(async () => {
      primaryTimer = null;
      const probe = createProbe();
      if (!probe || myGen !== gen || myProbe !== probeGen) return;
      const res = await dial("whep", opts.whepUrl, probe, () => undefined);
      if (!res.ok || !res.session || myGen !== gen || myProbe !== probeGen) {
        if (res.session) void Promise.resolve(res.session.close()).catch(() => undefined);
        if (myGen === gen && myProbe === probeGen) schedulePrimaryRetry(myGen);
        return;
      }
      const probeSession = res.session;
      const presented = await new Promise<boolean>((resolve) => {
        const stop = watchFrames(probe, () => {
          clearTimeout(timer);
          stop();
          resolve(true);
        });
        const timer = setTimeout(() => {
          stop();
          resolve(false);
        }, firstFrameMs);
      });
      if (!presented || myGen !== gen || myProbe !== probeGen) {
        void Promise.resolve(probeSession.close()).catch(() => undefined);
        if (myGen === gen && myProbe === probeGen) schedulePrimaryRetry(myGen);
        return;
      }
      // Adopt: the probe proved WHEP presents frames, so swap it in.
      const stream = probe.srcObject;
      probe.srcObject = null;
      teardown();
      const adoptGen = ++gen;
      index = whepRank;
      freezes = [];
      video.srcObject = stream;
      adoptedStream = stream;
      session = probeSession;
      emit({ state: "connecting", transport: "whep", highLatency: false, error: null });
      armSession(adoptGen);
    }, primaryRetryMs);
  }

  return {
    start() {
      if (running) return;
      running = true;
      freezes = [];
      if (order.length === 0) {
        emit({ state: "failed", transport: null, error: "No video transport configured." });
        return;
      }
      void dialIndex(0);
    },
    stop() {
      running = false;
      gen += 1;
      teardown();
    },
    snapshot: () => snap,
  };
}
