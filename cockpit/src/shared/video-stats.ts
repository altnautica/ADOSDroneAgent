// Receiver-side video counters: the WebRTC `inbound-rtp` video entry from
// `RTCPeerConnection.getStats()` and the frames the element presented
// (`requestVideoFrameCallback`'s `presentedFrames`), sampled once a second.
// Two samples give the rates a stall is diagnosed by: frames received,
// decoded and shown per second, frames dropped, the inter-frame delay's mean
// and spread, the jitter buffer's mean hold, and the receive bitrate. A
// decoded rate well above the shown rate is a renderer problem; a received
// rate that falls with the bytes is the link.

export interface InboundVideoCounters {
  framesReceived: number;
  framesDecoded: number;
  framesDropped: number;
  /** Seconds, summed over decoded frames. */
  totalInterFrameDelay: number;
  totalSquaredInterFrameDelay: number;
  /** Seconds, summed over emitted frames. */
  jitterBufferDelay: number;
  jitterBufferEmittedCount: number;
  bytesReceived: number;
  decoderImplementation: string | null;
}

export interface VideoStatsSample {
  /** Wall-clock ms. */
  at: number;
  /** Frames the element presented so far; null when not counted. */
  presentedFrames: number | null;
  /** Null for a transport with no receiver counters (HLS). */
  inbound: InboundVideoCounters | null;
}

/** Rates over one sampling interval; null where a counter is missing or went
 *  backwards (a new receiver). */
export interface VideoStats {
  receivedFps: number | null;
  decodedFps: number | null;
  shownFps: number | null;
  droppedFrames: number | null;
  interFrameMs: number | null;
  interFrameSdMs: number | null;
  jitterBufferMs: number | null;
  kbps: number | null;
  decoder: string | null;
}

const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);

/** The video receiver's counters, or null before the receiver exists. */
export async function readInboundVideo(pc: Pick<RTCPeerConnection, "getStats">): Promise<InboundVideoCounters | null> {
  const report = await pc.getStats();
  let found: InboundVideoCounters | null = null;
  report.forEach((entry: Record<string, unknown>) => {
    if (found || entry.type !== "inbound-rtp" || (entry.kind ?? entry.mediaType) !== "video") return;
    found = {
      framesReceived: num(entry.framesReceived),
      framesDecoded: num(entry.framesDecoded),
      framesDropped: num(entry.framesDropped),
      totalInterFrameDelay: num(entry.totalInterFrameDelay),
      totalSquaredInterFrameDelay: num(entry.totalSquaredInterFrameDelay),
      jitterBufferDelay: num(entry.jitterBufferDelay),
      jitterBufferEmittedCount: num(entry.jitterBufferEmittedCount),
      bytesReceived: num(entry.bytesReceived),
      decoderImplementation: typeof entry.decoderImplementation === "string" ? entry.decoderImplementation : null,
    };
  });
  return found;
}

export function videoStatsBetween(prev: VideoStatsSample, cur: VideoStatsSample): VideoStats {
  const seconds = (cur.at - prev.at) / 1000;
  /** A counter's growth, or null when it is missing or went backwards. */
  const grew = (a: number | null | undefined, b: number | null | undefined) =>
    a == null || b == null || b < a || seconds <= 0 ? null : b - a;
  const perSecond = (d: number | null) => (d === null ? null : d / seconds);
  const p = prev.inbound;
  const c = cur.inbound;

  const decoded = grew(p?.framesDecoded, c?.framesDecoded);
  const interSum = grew(p?.totalInterFrameDelay, c?.totalInterFrameDelay);
  const interSq = grew(p?.totalSquaredInterFrameDelay, c?.totalSquaredInterFrameDelay);
  let interFrameMs: number | null = null;
  let interFrameSdMs: number | null = null;
  if (decoded && interSum !== null && interSq !== null) {
    const mean = interSum / decoded;
    interFrameMs = mean * 1000;
    interFrameSdMs = Math.sqrt(Math.max(0, interSq / decoded - mean * mean)) * 1000;
  }
  const held = grew(p?.jitterBufferDelay, c?.jitterBufferDelay);
  const emitted = grew(p?.jitterBufferEmittedCount, c?.jitterBufferEmittedCount);
  const bytes = grew(p?.bytesReceived, c?.bytesReceived);

  return {
    receivedFps: perSecond(grew(p?.framesReceived, c?.framesReceived)),
    decodedFps: perSecond(decoded),
    shownFps: perSecond(grew(prev.presentedFrames, cur.presentedFrames)),
    droppedFrames: grew(p?.framesDropped, c?.framesDropped),
    interFrameMs,
    interFrameSdMs,
    jitterBufferMs: held !== null && emitted ? (held / emitted) * 1000 : null,
    kbps: bytes === null ? null : (bytes * 8) / seconds / 1000,
    decoder: c?.decoderImplementation ?? null,
  };
}
