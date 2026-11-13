// The one video policy both agent SPAs play through.
//
// A transport that negotiated is only a candidate: the feed is `live` once a
// frame has actually been presented, and it is `frozen` once frames stop for
// `noFrameMs`. Frame progress comes from `requestVideoFrameCallback` (with a
// `timeupdate` fallback where it is missing), never from the handshake.
//
// Policy, per failure exactly one step:
//   - dial `order[0]`; a dial failure, a lost session or no first frame moves
//     to the next transport (while recovering, see below, only a lost
//     session does); when the last one fails the state is `failed` and
//     the cascade restarts from the top after `retryMs`, doubling on each
//     consecutive full failure up to `maxRetryMs` (reset by a presented frame),
//     so a node with no video costs a few requests a minute, not a tight loop;
//   - a feed that presented a frame within the last `recoverWindowMs` is
//     recovering, not absent: a source outage ends with the agent's path
//     coming back, and every dial is a cheap probe of whether it has (one
//     the path cannot serve is refused at once), so the restart comes every
//     `recoverRetryMs` without doubling, and a WHEP session that shows no
//     first frame within `recoverFirstFrameMs` (the encoder sends a keyframe
//     at least every second) counts as failed, so frames are back about a
//     second after the path is. A WHEP dial refused or silent while
//     recovering (the encoder respawning for a profile change) restarts from
//     the top rather than moving to HLS;
//   - a freeze (no frame shown for `noFrameMs`) re-dials the current
//     transport, but only once the receiver's byte count has also stopped
//     growing for `noFrameMs`: a picture that holds while bytes still arrive
//     is a decoder waiting for a keyframe (an encoder restarting with a new
//     resolution), which the same session recovers from, so it shows `frozen`
//     and is re-dialled only if it stays frozen for `decodeStallMs`. A freeze
//     never moves down the order: a lost session (ICE or the peer connection
//     `failed`) does, and so does a dial failure or a session that never shows
//     a first frame outside recovery, flagged `highLatency` when that step
//     trades WHEP for HLS;
//   - while playing on a transport ranked below WHEP, WHEP is re-tried every
//     `primaryRetryMs` on an off-screen element and adopted only once it
//     presents a frame, so a viewer whose WHEP never works keeps its picture.

import { startHls } from "./hls";
import i18n from "./i18n";
import { readInboundVideo, videoStatsBetween, type InboundVideoCounters, type VideoStats, type VideoStatsSample } from "./video-stats";
import { startWhep } from "./whep";

export type TransportKind = "whep" | "hls";
export type VideoFeedState = "connecting" | "live" | "frozen" | "failed";

export interface VideoTransportSnapshot {
  state: VideoFeedState;
  transport: TransportKind | null;
  /** True while playing HLS because WHEP failed. */
  highLatency: boolean;
  error: string | null;
  width: number | null;
  height: number | null;
}

export interface TransportSession {
  close: () => void | Promise<void>;
  /** The receiver's video counters (WHEP); absent for HLS. */
  inboundVideo?: () => Promise<InboundVideoCounters | null>;
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

/** Calls `onFrame` per presented frame, with the element's running
 *  `presentedFrames` count where the browser reports it. */
export type FrameWatcher = (video: HTMLVideoElement, onFrame: (presentedFrames?: number) => void) => () => void;

export interface VideoTransportOptions {
  order: TransportKind[];
  whepUrl: string;
  hlsUrl: string;
  video: HTMLVideoElement;
  noFrameMs?: number;
  firstFrameMs?: number;
  retryMs?: number;
  maxRetryMs?: number;
  /** How long after the last presented frame the feed counts as recovering. */
  recoverWindowMs?: number;
  /** The restart delay while recovering. */
  recoverRetryMs?: number;
  /** The WHEP first-frame wait while recovering. */
  recoverFirstFrameMs?: number;
  /** How long a picture may stay frozen while bytes still arrive before the
   *  session is re-dialled anyway. */
  decodeStallMs?: number;
  primaryRetryMs?: number;
  onChange?: (snapshot: VideoTransportSnapshot) => void;
  /** The receive rates, once a second while a session plays; null on stop. */
  onStats?: (stats: VideoStats | null) => void;
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
/** A feed that presented a frame this recently is recovering (see the policy). */
export const VIDEO_RECOVER_WINDOW_MS = 60_000;
/** A decoder that shows nothing this long while bytes arrive is stuck. */
export const VIDEO_DECODE_STALL_MS = 8000;
/** The background WHEP retry while playing a lower-ranked transport. */
export const VIDEO_PRIMARY_RETRY_MS = 10_000;
const STATS_INTERVAL_MS = 1000;

/** The part of a peer connection `watchPeerFailure` reads. */
export interface PeerStateSource {
  readonly connectionState: string;
  readonly iceConnectionState: string;
  addEventListener: (type: "connectionstatechange" | "iceconnectionstatechange", cb: () => void) => void;
}

/** Calls `onLost` once, when ICE or the peer connection reports `failed`
 *  (`disconnected` is transient and recovers on its own). */
export function watchPeerFailure(pc: PeerStateSource, onLost: () => void): void {
  let lost = false;
  const check = () => {
    if (lost || (pc.connectionState !== "failed" && pc.iceConnectionState !== "failed")) return;
    lost = true;
    onLost();
  };
  pc.addEventListener("connectionstatechange", check);
  pc.addEventListener("iceconnectionstatechange", check);
}

/** The production dialer: a WHEP session reports a failed ICE or peer
 *  connection as lost and exposes its receiver counters; an HLS session
 *  reports hls.js giving up. */
export const defaultDialer: TransportDialer = async (kind, url, video, onLost) => {
  if (kind === "hls") return startHls(url, video, onLost);
  const res = await startWhep(url, video);
  if (!res.session) return res;
  const { pc, close } = res.session;
  watchPeerFailure(pc, onLost);
  return { ok: res.ok, session: { close, inboundVideo: () => readInboundVideo(pc) } };
};

type FrameCallbackVideo = HTMLVideoElement & {
  requestVideoFrameCallback?: (cb: (now: number, meta?: { presentedFrames?: number }) => void) => number;
  cancelVideoFrameCallback?: (handle: number) => void;
};

/** Frame progress from `requestVideoFrameCallback`, else `timeupdate`. */
export const defaultFrameWatcher: FrameWatcher = (video, onFrame) => {
  const v = video as FrameCallbackVideo;
  if (typeof v.requestVideoFrameCallback === "function") {
    let handle = 0;
    let stopped = false;
    const loop = (_now: number, meta?: { presentedFrames?: number }) => {
      if (stopped) return;
      onFrame(meta?.presentedFrames);
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
    recoverWindowMs = VIDEO_RECOVER_WINDOW_MS,
    recoverRetryMs = 1000,
    recoverFirstFrameMs = 3000,
    decodeStallMs = VIDEO_DECODE_STALL_MS,
    primaryRetryMs = VIDEO_PRIMARY_RETRY_MS,
    onChange,
    onStats,
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
  /** When any session last presented a frame; 0 before the first. */
  let lastLiveAt = 0;
  let sessionStartedAt = 0;
  let sawFrame = false;
  let fullFailures = 0;
  /** Frames presented in this session: the element's own count, else ours. */
  let presentedFrames = 0;
  let prevSample: VideoStatsSample | null = null;
  /** When a sample last saw the receiver's byte count grow; 0 for never. */
  let lastBytesAt = 0;
  let watchdog: ReturnType<typeof setInterval> | null = null;
  let statsTimer: ReturnType<typeof setInterval> | null = null;
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
    clearInterval(statsTimer ?? undefined);
    clearTimeout(retryTimer ?? undefined);
    clearTimeout(primaryTimer ?? undefined);
    watchdog = statsTimer = retryTimer = primaryTimer = null;
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

  const recovering = () => lastLiveAt > 0 && Date.now() - lastLiveAt < recoverWindowMs;

  const onFrame = (presented?: number) => {
    lastFrameAt = Date.now();
    lastLiveAt = lastFrameAt;
    sawFrame = true;
    fullFailures = 0;
    presentedFrames = typeof presented === "number" ? presented : presentedFrames + 1;
    const w = video.videoWidth > 0 ? video.videoWidth : null;
    const h = video.videoHeight > 0 ? video.videoHeight : null;
    if (snap.state !== "live" || snap.width !== w || snap.height !== h) {
      emit({ state: "live", error: null, width: w, height: h });
    }
  };

  /** One stats sample: the receiver's counters and the frames shown. */
  const sampleStats = async (myGen: number) => {
    let inbound: InboundVideoCounters | null = null;
    try {
      inbound = (await session?.inboundVideo?.()) ?? null;
    } catch {
      inbound = null;
    }
    if (myGen !== gen) return;
    const cur: VideoStatsSample = { at: Date.now(), presentedFrames, inbound };
    if (prevSample) {
      const before = prevSample.inbound?.bytesReceived;
      if (inbound && before !== undefined && inbound.bytesReceived > before) lastBytesAt = cur.at;
      onStats?.(videoStatsBetween(prevSample, cur));
    }
    prevSample = cur;
  };

  const armSession = (myGen: number) => {
    sessionStartedAt = Date.now();
    lastFrameAt = 0;
    sawFrame = false;
    presentedFrames = 0;
    prevSample = null;
    lastBytesAt = 0;
    const firstWait = order[index] === "whep" && recovering() ? Math.min(firstFrameMs, recoverFirstFrameMs) : firstFrameMs;
    stopFrames = watchFrames(video, onFrame);
    const tick = Math.max(100, Math.min(500, Math.floor(noFrameMs / 2)));
    watchdog = setInterval(() => {
      if (myGen !== gen) return;
      const now = Date.now();
      if (!sawFrame) {
        if (now - sessionStartedAt >= firstWait) failCurrent(myGen, i18n.t("video.error.noFrames"));
        return;
      }
      const stalledFor = now - lastFrameAt;
      if (stalledFor < noFrameMs) return;
      // Bytes still arriving: the decoder is behind, not the session gone.
      const bytesFlowing = lastBytesAt > 0 && now - lastBytesAt < noFrameMs;
      if (bytesFlowing && stalledFor < decodeStallMs) {
        if (snap.state !== "frozen") emit({ state: "frozen" });
        return;
      }
      onFreeze(myGen);
    }, tick);
    void sampleStats(myGen);
    statsTimer = setInterval(() => void sampleStats(myGen), STATS_INTERVAL_MS);
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
      if (myGen === gen) failCurrent(myGen, i18n.t("video.error.sessionLost"), true);
    });
    if (myGen !== gen || !running) {
      if (result.session) void Promise.resolve(result.session.close()).catch(() => undefined);
      return;
    }
    if (!result.ok || !result.session) {
      failCurrent(myGen, result.error ?? i18n.t("video.error.transportFailed"));
      return;
    }
    session = result.session;
    armSession(myGen);
  };

  /** One step down the order, or `failed` plus a restart from the top. A
   *  recovering WHEP feed steps down only for a lost session: a refused or
   *  silent dial then is the agent's path coming back, not WHEP failing. */
  function failCurrent(myGen: number, error: string, lost = false) {
    if (myGen !== gen || !running) return;
    const holdWhep = !lost && order[index] === "whep" && recovering();
    if (index + 1 < order.length && !holdWhep) {
      void dialIndex(index + 1);
      return;
    }
    teardown();
    gen += 1;
    emit({ state: "failed", error, width: null, height: null });
    // The doubling counts only the restarts after the recovery window, so it
    // starts again from `retryMs` once the feed counts as absent.
    let delay = recoverRetryMs;
    if (!recovering()) {
      delay = Math.min(maxRetryMs, retryMs * 2 ** fullFailures);
      fullFailures += 1;
    }
    retryTimer = setTimeout(() => {
      retryTimer = null;
      if (running) void dialIndex(0);
    }, delay);
  }

  /** A freeze re-dials the transport it froze on; it never moves down. */
  function onFreeze(myGen: number) {
    if (myGen !== gen) return;
    emit({ state: "frozen" });
    void dialIndex(index, "frozen");
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
      onStats?.(null);
    },
    snapshot: () => snap,
  };
}
